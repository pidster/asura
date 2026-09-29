//! Bounded observation pipeline. This owner never starts models or owns storage IO.
use asura_control::pb;
use asura_storage::sensors::{
    self as record, HoldReason, Id, Observation, Payload, ProjectState, Proposal, ProposalState,
    Purpose, Source,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
const MAX_PROJECTS: usize = 64;
const IDLE: Duration = Duration::from_secs(300);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Invalid,
    Busy,
    Limit,
    Stale,
    Conflict,
    Unavailable,
    Unconfirmed,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct StatusSnapshot {
    pub lifecycle: i32,
    pub installation: i32,
    pub reason: i32,
}
#[derive(Clone, Debug)]
pub(crate) struct Capture {
    pub observation: Observation,
    pub durable: bool,
}
#[derive(Clone, Debug)]
pub(crate) struct Write {
    pub expected_revision: u64,
    pub state: ProjectState,
}
struct Project {
    committed: ProjectState,
    working: ProjectState,
    dirty: bool,
    writing: Option<Write>,
    uncertain: bool,
    clock_uncertain: bool,
    clock_recheck: Instant,
    last_activity: Instant,
    last_activity_ms: u64,
    active: u32,
    idle_emitted: bool,
    debounce: Option<(Instant, Instant)>,
    pending: Vec<Id>,
    sequence: u64,
    anchor: Instant,
    wall_anchor: u64,
}
impl Project {
    /// Hold freshness decisions during rollback without leaving an expired timer armed.
    fn sample_clock(&mut self, now: Instant, wall_ms: u64) -> bool {
        if wall_ms < self.wall_anchor.max(self.working.last_received_ms) {
            self.clock_uncertain = true;
            self.clock_recheck = now + Duration::from_secs(1);
            return false;
        }
        self.anchor = now;
        self.wall_anchor = wall_ms;
        self.clock_uncertain = false;
        true
    }
}
pub(crate) struct Owner {
    epoch: Id,
    projects: BTreeMap<Id, Project>,
    status: StatusSnapshot,
    closing: bool,
}
impl Owner {
    pub fn new(epoch: Id, status: StatusSnapshot) -> Result<Self, Error> {
        if epoch == [0; 16] {
            return Err(Error::Invalid);
        }
        Ok(Self {
            epoch,
            projects: BTreeMap::new(),
            status,
            closing: false,
        })
    }
    pub fn status(&mut self, status: StatusSnapshot) {
        self.status = status;
    }
    pub fn restore(
        &mut self,
        mut state: ProjectState,
        now: Instant,
        wall_ms: u64,
    ) -> Result<(), Error> {
        state.validate().map_err(map_error)?;
        if self.projects.contains_key(&state.project) {
            return Err(Error::Busy);
        }
        if self.projects.len() >= MAX_PROJECTS {
            return Err(Error::Limit);
        }
        let original = state.clone();
        for proposal in &mut state.proposals {
            if proposal.state == ProposalState::Held {
                if wall_ms < state.last_received_ms {
                    proposal.reason = HoldReason::ClockUncertain;
                } else if wall_ms >= proposal.expires_ms {
                    proposal.state = ProposalState::Expired;
                    proposal.reason = HoldReason::Expired;
                }
            }
        }
        let dirty = state != original;
        let clock_uncertain = wall_ms < original.last_received_ms;
        self.projects.insert(
            state.project,
            Project {
                committed: original,
                working: state,
                dirty,
                writing: None,
                uncertain: false,
                clock_uncertain,
                clock_recheck: now + Duration::from_secs(1),
                last_activity: now,
                last_activity_ms: wall_ms,
                active: 0,
                idle_emitted: false,
                debounce: None,
                pending: vec![],
                sequence: 0,
                anchor: now,
                wall_anchor: wall_ms,
            },
        );
        Ok(())
    }
    /// Seed foreground state from canonical replay before enabling idle observation.
    pub fn active_foreground(&mut self, project: Id, count: u32) -> Result<(), Error> {
        self.projects
            .get_mut(&project)
            .ok_or(Error::Unavailable)?
            .active = count;
        Ok(())
    }
    /// Retry only after the canonical writer has resolved its previous operation.
    pub fn pending_write(&self) -> Option<&Write> {
        self.projects
            .values()
            .find_map(|project| project.writing.as_ref())
    }
    pub fn ingest(
        &mut self,
        value: Observation,
        now: Instant,
        wall_ms: u64,
    ) -> Result<Capture, Error> {
        if self.closing {
            return Err(Error::Unavailable);
        }
        value.validate().map_err(map_error)?;
        if value.source_epoch != self.epoch
            || value.received_ms > wall_ms
            || wall_ms >= value.expires_ms
        {
            return Err(Error::Stale);
        }
        let project = self
            .projects
            .get_mut(&value.project)
            .ok_or(Error::Unavailable)?;
        if let Some(previous) = project
            .working
            .observations
            .iter()
            .find(|old| old.id == value.id)
        {
            if previous != &value {
                return Err(Error::Conflict);
            }
            return Ok(Capture {
                observation: previous.clone(),
                durable: project
                    .committed
                    .observations
                    .iter()
                    .any(|old| old == previous),
            });
        }
        if value.source == Source::Activity
            && project.working.observations.iter().any(|old| {
                old.source == Source::Activity
                    && old.source_epoch == value.source_epoch
                    && old.sequence > value.sequence
            })
        {
            return Err(Error::Stale);
        }
        if project.uncertain {
            return Err(Error::Unconfirmed);
        }
        if project.writing.is_some() {
            return Err(Error::Busy);
        }
        if !project.sample_clock(now, wall_ms) {
            return Err(Error::Stale);
        }
        prune(&mut project.working, wall_ms);
        project
            .pending
            .retain(|id| project.working.observations.iter().any(|o| &o.id == id));
        if project.pending.is_empty() {
            project.debounce = None;
        }
        if project.working.observations.len() >= record::MAX_OBSERVATIONS {
            return Err(Error::Limit);
        }
        if let Payload::Activity {
            active_foreground, ..
        } = value.payload
            && !value.background
            && value.causal_hops <= 2
        {
            if project.pending.len() >= record::MAX_REFERENCES {
                return Err(Error::Limit);
            }
            project.active = active_foreground;
            project.last_activity = now;
            project.last_activity_ms = wall_ms;
            project.idle_emitted = false;
            project.clock_uncertain = false;
            for proposal in &mut project.working.proposals {
                if proposal.purpose == Purpose::Reflection && proposal.state == ProposalState::Held
                {
                    proposal.state = ProposalState::Invalidated;
                    proposal.reason = HoldReason::ActivityResumed;
                }
            }
            project.pending.push(value.id);
            let first = project.debounce.map_or(now, |(_, first)| first);
            project.debounce = Some((
                (now + Duration::from_millis(250)).min(first + Duration::from_secs(2)),
                first,
            ));
        }
        project.working.last_received_ms = value.received_ms;
        project.working.observations.push(value.clone());
        project.dirty = true;
        Ok(Capture {
            observation: value,
            durable: false,
        })
    }
    pub fn capture_status(
        &mut self,
        project: Id,
        operation: Id,
        generation: u64,
        ordinal: u32,
        now: Instant,
        wall_ms: u64,
    ) -> Result<Capture, Error> {
        if self.closing {
            return Err(Error::Unavailable);
        }
        let mut hash = Sha256::new();
        hash.update(operation);
        hash.update(generation.to_be_bytes());
        hash.update(ordinal.to_be_bytes());
        let sequence = u64::from_be_bytes(hash.finalize()[..8].try_into().unwrap()).max(1);
        let id = record::observation_id(project, Source::ServiceStatus, self.epoch, sequence);
        if let Some(state) = self.projects.get(&project)
            && let Some(value) = state.working.observations.iter().find(|value| matches!(value.payload,
                Payload::ServiceStatus { operation: old_operation, generation: old_generation, ordinal: old_ordinal, .. }
                    if old_operation == operation && old_generation == generation && old_ordinal == ordinal)) {
                if wall_ms < value.received_ms || wall_ms >= value.expires_ms { return Err(Error::Stale); }
                return Ok(Capture { observation: value.clone(), durable: state.committed.observations.iter().any(|old| old == value) });
        }
        self.ingest(
            Observation {
                id,
                project,
                source: Source::ServiceStatus,
                source_epoch: self.epoch,
                sequence,
                observed_ms: wall_ms,
                received_ms: wall_ms,
                expires_ms: wall_ms.checked_add(30_000).ok_or(Error::Invalid)?,
                causal_root: Some(operation),
                background: false,
                causal_hops: 0,
                payload: Payload::ServiceStatus {
                    lifecycle: self.status.lifecycle,
                    installation: self.status.installation,
                    reason: self.status.reason,
                    operation,
                    generation,
                    ordinal,
                },
            },
            now,
            wall_ms,
        )
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.closing {
            return None;
        }
        self.projects
            .values()
            .filter(|p| !p.uncertain && p.writing.is_none())
            .flat_map(|p| {
                let held = p.clock_uncertain;
                let regular = p
                    .debounce
                    .map(|(at, _)| at)
                    .into_iter()
                    .chain((!p.idle_emitted && p.active == 0).then_some(p.last_activity + IDLE))
                    .chain(
                        p.working
                            .proposals
                            .iter()
                            .filter(|proposal| proposal.state == ProposalState::Held)
                            .map(|proposal| {
                                p.anchor
                                    + Duration::from_millis(
                                        proposal.expires_ms.saturating_sub(p.wall_anchor),
                                    )
                            }),
                    )
                    .filter(move |_| !held);
                regular.chain(held.then_some(p.clock_recheck))
            })
            .min()
    }
    pub fn poll(&mut self, now: Instant, wall_ms: u64) {
        if self.closing {
            return;
        }
        for (&id, project) in &mut self.projects {
            if project.uncertain || project.writing.is_some() {
                continue;
            }
            if !project.sample_clock(now, wall_ms) {
                continue;
            }
            for proposal in &mut project.working.proposals {
                if proposal.state == ProposalState::Held && wall_ms >= proposal.expires_ms {
                    proposal.state = ProposalState::Expired;
                    proposal.reason = HoldReason::Expired;
                    project.dirty = true;
                }
            }
            if project.debounce.is_some_and(|(at, _)| now >= at) {
                project.pending.retain(|id| {
                    project
                        .working
                        .observations
                        .iter()
                        .any(|o| &o.id == id && wall_ms < o.expires_ms)
                });
                if propose(
                    &mut project.working,
                    Purpose::Consolidation,
                    &project.pending,
                    wall_ms,
                )
                .is_ok()
                {
                    project.pending.clear();
                    project.debounce = None;
                    project.dirty = true;
                } else {
                    project.debounce = None;
                    project.pending.clear();
                }
            }
            if !project.idle_emitted && project.active == 0 && now >= project.last_activity + IDLE {
                prune(&mut project.working, wall_ms);
                // A saturated state has no timer retry loop. New activity or reload can retry.
                project.idle_emitted = true;
                if project.working.observations.len() >= record::MAX_OBSERVATIONS {
                    continue;
                }
                let Some(sequence) = project.sequence.checked_add(1) else {
                    continue;
                };
                project.sequence = sequence;
                let Some(expires) = wall_ms.checked_add(300_000) else {
                    continue;
                };
                let observation = Observation {
                    id: record::observation_id(id, Source::Idle, self.epoch, sequence),
                    project: id,
                    source: Source::Idle,
                    source_epoch: self.epoch,
                    sequence,
                    observed_ms: wall_ms,
                    received_ms: wall_ms,
                    expires_ms: expires,
                    causal_root: None,
                    background: false,
                    causal_hops: 0,
                    payload: Payload::Idle {
                        since_ms: project.last_activity_ms,
                    },
                };
                let observation_id = observation.id;
                project.working.observations.push(observation);
                project.working.last_received_ms = wall_ms;
                let _ = propose(
                    &mut project.working,
                    Purpose::Reflection,
                    &[observation_id],
                    wall_ms,
                );
                project.dirty = true;
            }
        }
    }
    pub fn take_write(&mut self) -> Option<Write> {
        if self
            .projects
            .values()
            .any(|p| p.writing.is_some() && !p.uncertain)
        {
            return None;
        }
        let project = self
            .projects
            .values_mut()
            .find(|p| p.dirty && !p.uncertain && p.debounce.is_none())?;
        let mut state = project.working.clone();
        state.revision = state.revision.checked_add(1)?;
        state.write_id = asura_platform::random_id();
        if state.validate().is_err() {
            project.uncertain = true;
            return None;
        }
        let write = Write {
            expected_revision: project.committed.revision,
            state,
        };
        project.writing = Some(write.clone());
        Some(write)
    }
    pub fn persisted(
        &mut self,
        project_id: Id,
        write_id: Id,
        result: Result<(), Error>,
    ) -> Result<(), Error> {
        let project = self
            .projects
            .get_mut(&project_id)
            .ok_or(Error::Unavailable)?;
        let write = project.writing.as_ref().ok_or(Error::Invalid)?;
        if write.state.write_id != write_id {
            return Err(Error::Stale);
        }
        match result {
            Ok(()) => {
                let state = project.writing.take().unwrap().state;
                project.committed = state.clone();
                project.working = state;
                project.dirty = false;
                Ok(())
            }
            Err(error) => {
                project.uncertain = true;
                Err(error)
            }
        }
    }
    /// The caller obtains this state through the serialized writer after the old
    /// operation settles. True confirms the original write; false requests retry
    /// of pending_write with exactly the same identity, never a replacement write.
    pub fn reconcile(&mut self, state: ProjectState) -> Result<bool, Error> {
        state.validate().map_err(map_error)?;
        let project = self
            .projects
            .get_mut(&state.project)
            .ok_or(Error::Unavailable)?;
        let write = project.writing.as_ref().ok_or(Error::Invalid)?;
        if state == write.state {
            project.committed = state.clone();
            project.working = state;
            project.writing = None;
            project.uncertain = false;
            project.dirty = false;
            return Ok(true);
        }
        if state == project.committed && state.revision == write.expected_revision {
            return Ok(false);
        }
        Err(Error::Conflict)
    }
    pub fn snapshot(&self, project: Id) -> Option<&ProjectState> {
        self.projects.get(&project).map(|p| &p.committed)
    }
    /// Bounded committed projection: no IO and no publication of pending evidence.
    pub fn inspect(&self, query: &pb::SensorsInspect) -> pb::SensorsReply {
        let project_id: Id = query
            .project_id
            .as_deref()
            .and_then(|v| v.try_into().ok())
            .unwrap_or([0; 16]);
        let Some(project) = self.projects.get(&project_id) else {
            return inspection_error("sensor_loading");
        };
        let state = &project.committed;
        if query
            .revision
            .is_some_and(|revision| revision != state.revision)
        {
            return inspection_error("revision_conflict");
        }
        let offset = query.offset.unwrap_or(0) as usize;
        let limit = query.limit.unwrap_or(0) as usize;
        if offset > state.observations.len()
            || !(1..=16).contains(&limit)
            || (offset > 0 && query.revision.is_none())
        {
            return inspection_error("invalid_offset");
        }
        let end = (offset + limit).min(state.observations.len());
        pb::SensorsReply {
            project_id: Some(project_id.to_vec()),
            revision: Some(state.revision),
            total_observations: Some(state.observations.len() as u32),
            offset: Some(offset as u32),
            next_offset: (end < state.observations.len()).then_some(end as u32),
            pending_persistence: Some(project.dirty || project.writing.is_some()),
            intake_unavailable: Some(project.uncertain),
            clock_uncertain: Some(project.clock_uncertain),
            observations: state.observations[offset..end]
                .iter()
                .map(observation_summary)
                .collect(),
            proposals: state
                .proposals
                .iter()
                .map(|p| pb::SensorProposal {
                    id: Some(p.id.to_vec()),
                    purpose: Some(match p.purpose {
                        Purpose::Consolidation => 1,
                        Purpose::Reflection => 2,
                    }),
                    state: Some(match p.state {
                        ProposalState::Held => 1,
                        ProposalState::Expired => 2,
                        ProposalState::Invalidated => 3,
                    }),
                    reason: Some(match p.reason {
                        HoldReason::SensorAdmissionUnavailable => 1,
                        HoldReason::ClockUncertain => 2,
                        HoldReason::ActivityResumed => 3,
                        HoldReason::Expired => 4,
                    }),
                    observations: p
                        .observations
                        .iter()
                        .map(|id| pb::SensorReference {
                            id: Some(id.to_vec()),
                        })
                        .collect(),
                    omitted: Some(p.omitted),
                    created_ms: Some(p.created_ms),
                    expires_ms: Some(p.expires_ms),
                })
                .collect(),
            error: None,
        }
    }
    pub fn disable(&mut self, project: Id) {
        if let Some(project) = self.projects.get_mut(&project) {
            project.uncertain = true;
            project.writing = None;
            project.dirty = false;
            project.debounce = None;
        }
    }
    pub fn shutdown(&mut self) {
        if self.closing {
            return;
        }
        for project in self.projects.values_mut() {
            if project.debounce.take().is_some() && !project.pending.is_empty() {
                let _ = propose(
                    &mut project.working,
                    Purpose::Consolidation,
                    &project.pending,
                    project.last_activity_ms,
                );
                project.pending.clear();
                project.dirty = true;
            }
        }
        self.closing = true;
    }
    pub fn settled(&self) -> bool {
        self.projects
            .values()
            .all(|p| p.writing.is_none() && (p.uncertain || !p.dirty))
    }
}
pub(crate) fn inspection_error(reason: &str) -> pb::SensorsReply {
    pb::SensorsReply {
        error: Some(reason.into()),
        ..Default::default()
    }
}
fn observation_summary(value: &Observation) -> pb::SensorObservation {
    let mut row = pb::SensorObservation {
        id: Some(value.id.to_vec()),
        source: Some(match value.source {
            Source::Activity => 1,
            Source::Idle => 2,
            Source::ServiceStatus => 3,
        }),
        source_epoch: Some(value.source_epoch.to_vec()),
        sequence: Some(value.sequence),
        observed_ms: Some(value.observed_ms),
        received_ms: Some(value.received_ms),
        expires_ms: Some(value.expires_ms),
        causal_root: value.causal_root.map(|id| id.to_vec()),
        background: Some(value.background),
        causal_hops: Some(u32::from(value.causal_hops)),
        ..Default::default()
    };
    match value.payload {
        Payload::Activity {
            operation,
            generation,
            phase,
            active_foreground,
        } => {
            row.operation = Some(operation.to_vec());
            row.generation = Some(generation);
            row.phase = Some(match phase {
                record::Phase::Accepted => 1,
                record::Phase::Settled => 2,
            });
            row.active_foreground = Some(active_foreground);
        }
        Payload::Idle { since_ms } => row.since_ms = Some(since_ms),
        Payload::ServiceStatus {
            lifecycle,
            installation,
            reason,
            operation,
            generation,
            ordinal,
        } => {
            row.lifecycle = Some(lifecycle as u32);
            row.installation = Some(installation as u32);
            row.status_reason = Some(reason as u32);
            row.operation = Some(operation.to_vec());
            row.generation = Some(generation);
            row.ordinal = Some(ordinal);
        }
    }
    row
}
fn prune(state: &mut ProjectState, wall_ms: u64) {
    state
        .proposals
        .retain(|p| p.state == ProposalState::Held && p.expires_ms > wall_ms);
    state.observations.retain(|o| {
        o.expires_ms > wall_ms
            || state
                .proposals
                .iter()
                .any(|p| p.observations.contains(&o.id))
    });
}
fn propose(
    state: &mut ProjectState,
    purpose: Purpose,
    ids: &[Id],
    wall_ms: u64,
) -> Result<(), Error> {
    if ids.is_empty() {
        return Ok(());
    }
    if let Some(proposal) = state
        .proposals
        .iter_mut()
        .find(|p| p.purpose == purpose && p.state == ProposalState::Held && p.expires_ms > wall_ms)
    {
        for id in ids {
            if !proposal.observations.contains(id) {
                if proposal.observations.len() < record::MAX_REFERENCES {
                    proposal.observations.push(*id);
                } else {
                    proposal.omitted = proposal.omitted.saturating_add(1);
                }
            }
        }
        return Ok(());
    }
    if state.proposals.len() >= record::MAX_PROPOSALS {
        return Err(Error::Limit);
    }
    let expires = wall_ms.checked_add(300_000).ok_or(Error::Invalid)?;
    state.proposals.push(Proposal {
        id: asura_platform::random_id(),
        purpose,
        state: ProposalState::Held,
        reason: HoldReason::SensorAdmissionUnavailable,
        observations: ids.to_vec(),
        omitted: 0,
        created_ms: wall_ms,
        expires_ms: expires,
    });
    Ok(())
}
fn map_error(error: record::Error) -> Error {
    match error {
        record::Error::Limit => Error::Limit,
        record::Error::Conflict => Error::Conflict,
        record::Error::Stale => Error::Stale,
        record::Error::Unavailable => Error::Unavailable,
        record::Error::Unconfirmed => Error::Unconfirmed,
        record::Error::Invalid => Error::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owner(now: Instant) -> Owner {
        let mut owner = Owner::new(
            [2; 16],
            StatusSnapshot {
                lifecycle: 1,
                installation: 1,
                reason: 0,
            },
        )
        .unwrap();
        owner
            .restore(ProjectState::empty([1; 16]), now, 1000)
            .unwrap();
        owner
    }
    fn activity(sequence: u64, wall: u64, background: bool) -> Observation {
        Observation {
            id: record::observation_id([1; 16], Source::Activity, [2; 16], sequence),
            project: [1; 16],
            source: Source::Activity,
            source_epoch: [2; 16],
            sequence,
            observed_ms: wall,
            received_ms: wall,
            expires_ms: wall + 300000,
            causal_root: Some([3; 16]),
            background,
            causal_hops: 0,
            payload: Payload::Activity {
                operation: [3; 16],
                generation: 1,
                phase: record::Phase::Settled,
                active_foreground: 0,
            },
        }
    }
    fn commit(owner: &mut Owner) {
        let write = owner.take_write().unwrap();
        write.state.validate().unwrap();
        owner
            .persisted(write.state.project, write.state.write_id, Ok(()))
            .unwrap();
    }
    fn query(offset: u32, revision: Option<u64>) -> pb::SensorsInspect {
        pb::SensorsInspect {
            project_id: Some(vec![1; 16]),
            offset: Some(offset),
            limit: Some(1),
            revision,
        }
    }
    #[test]
    fn inspection_never_publishes_unconfirmed_evidence_and_reports_failure() {
        let now = Instant::now();
        let mut owner = owner(now);
        owner.ingest(activity(1, 1000, false), now, 1000).unwrap();
        let pending = owner.inspect(&query(0, None));
        assert!(pending.observations.is_empty());
        assert_eq!(pending.pending_persistence, Some(true));
        owner.poll(now + Duration::from_millis(250), 1250);
        let write = owner.take_write().unwrap();
        assert!(owner.inspect(&query(0, None)).observations.is_empty());
        owner
            .persisted([1; 16], write.state.write_id, Ok(()))
            .unwrap();
        let ready = owner.inspect(&query(0, None));
        assert_eq!(ready.observations.len(), 1);
        assert_eq!(ready.proposals.len(), 1);
        assert_eq!(ready.proposals[0].reason, Some(1));
        assert_eq!(ready.pending_persistence, Some(false));
        owner.disable([1; 16]);
        let failed = owner.inspect(&query(0, None));
        assert_eq!(failed.intake_unavailable, Some(true));
        assert_eq!(failed.observations, ready.observations);
        let envelope = pb::Envelope {
            service_epoch: Some(vec![2; 16]),
            attachment_id: Some(vec![3; 16]),
            request_counter: Some(1),
            body: Some(pb::envelope::Body::SensorsReply(failed)),
        };
        asura_control::validate_semantics(&envelope, asura_control::Direction::ServerToClient)
            .unwrap();
    }
    #[test]
    fn inspection_pagination_fences_revision_and_recovers_committed_records() {
        let now = Instant::now();
        let mut owner = owner(now);
        for sequence in 1..=3 {
            owner
                .ingest(activity(sequence, 1000, true), now, 1000)
                .unwrap();
        }
        owner.poll(now + Duration::from_millis(250), 1250);
        commit(&mut owner);
        let first = owner.inspect(&query(0, None));
        assert_eq!(first.total_observations, Some(3));
        assert_eq!(first.next_offset, Some(1));
        let second = owner.inspect(&query(1, first.revision));
        assert_ne!(first.observations[0].id, second.observations[0].id);
        assert_eq!(
            owner.inspect(&query(1, Some(999))).error.as_deref(),
            Some("revision_conflict")
        );
        assert_eq!(
            owner.inspect(&query(4, first.revision)).error.as_deref(),
            Some("invalid_offset")
        );
        let state = owner.snapshot([1; 16]).unwrap().clone();
        let mut recovered = Owner::new(
            [4; 16],
            StatusSnapshot {
                lifecycle: 1,
                installation: 1,
                reason: 0,
            },
        )
        .unwrap();
        recovered.restore(state, now, 900).unwrap();
        let restored = recovered.inspect(&query(0, None));
        assert_eq!(restored.observations, first.observations);
        assert_eq!(restored.clock_uncertain, Some(true));
    }
    #[test]
    fn activity_debounces_and_ack_requires_storage() {
        let now = Instant::now();
        let mut owner = owner(now);
        let value = activity(1, 1000, false);
        assert!(!owner.ingest(value.clone(), now, 1000).unwrap().durable);
        assert!(owner.take_write().is_none());
        owner.poll(now + Duration::from_millis(250), 1250);
        assert!(owner.snapshot([1; 16]).unwrap().observations.is_empty());
        commit(&mut owner);
        let snapshot = owner.snapshot([1; 16]).unwrap();
        assert_eq!(
            snapshot.proposals[0].reason,
            HoldReason::SensorAdmissionUnavailable
        );
        assert!(owner.ingest(value, now, 1250).unwrap().durable);
    }
    #[test]
    fn duplicate_conflict_and_uncertain_storage_never_admit_more() {
        let now = Instant::now();
        let mut owner = owner(now);
        owner.ingest(activity(1, 1000, true), now, 1000).unwrap();
        let mut conflict = activity(1, 1000, true);
        conflict.causal_hops = 1;
        assert!(matches!(
            owner.ingest(conflict, now, 1000),
            Err(Error::Conflict)
        ));
        let write = owner.take_write().unwrap();
        assert!(matches!(
            owner.ingest(activity(2, 1001, false), now, 1001),
            Err(Error::Busy)
        ));
        assert_eq!(
            owner.persisted([1; 16], write.state.write_id, Err(Error::Unconfirmed)),
            Err(Error::Unconfirmed)
        );
        assert!(matches!(
            owner.ingest(activity(2, 1001, false), now, 1001),
            Err(Error::Unconfirmed)
        ));
        assert_eq!(
            owner.pending_write().unwrap().state.write_id,
            write.state.write_id
        );
        owner.shutdown();
        assert!(!owner.settled());
    }
    #[test]
    fn idle_is_once_and_foreground_invalidates_it() {
        let now = Instant::now();
        let mut owner = owner(now);
        owner.poll(now + IDLE, 301000);
        commit(&mut owner);
        assert_eq!(owner.snapshot([1; 16]).unwrap().proposals.len(), 1);
        owner.poll(now + IDLE + Duration::from_secs(1), 302000);
        assert!(owner.take_write().is_none());
        owner
            .ingest(activity(1, 302000, false), now + IDLE, 302000)
            .unwrap();
        owner.poll(now + IDLE + Duration::from_millis(250), 302250);
        commit(&mut owner);
        assert!(
            owner
                .snapshot([1; 16])
                .unwrap()
                .proposals
                .iter()
                .any(|p| p.purpose == Purpose::Reflection && p.state == ProposalState::Invalidated)
        );
    }
    #[test]
    fn active_tasks_and_background_events_do_not_propose_work() {
        let now = Instant::now();
        let mut owner = owner(now);
        owner.active_foreground([1; 16], 1).unwrap();
        owner.ingest(activity(1, 1000, true), now, 1000).unwrap();
        commit(&mut owner);
        owner.poll(now + IDLE, 301000);
        assert!(owner.snapshot([1; 16]).unwrap().proposals.is_empty());
    }
    #[test]
    fn status_is_stable_scoped_and_has_no_prompt_or_path() {
        let now = Instant::now();
        let mut owner = owner(now);
        let value = owner
            .capture_status([1; 16], [3; 16], 1, 1, now, 1000)
            .unwrap();
        assert!(!value.durable);
        commit(&mut owner);
        let again = owner
            .capture_status([1; 16], [3; 16], 1, 1, now, 1100)
            .unwrap();
        assert!(again.durable);
        assert_eq!(value.observation, again.observation);
        assert!(matches!(
            owner.capture_status([9; 16], [3; 16], 1, 1, now, 1000),
            Err(Error::Unavailable)
        ));
        assert!(matches!(
            owner.capture_status([1; 16], [3; 16], 1, 1, now, 31000),
            Err(Error::Stale)
        ));
    }
    #[test]
    fn restart_does_not_count_downtime_and_clock_rollback_holds() {
        let now = Instant::now();
        let mut first = owner(now);
        first.poll(now + IDLE, 301000);
        commit(&mut first);
        let state = first.snapshot([1; 16]).unwrap().clone();
        let mut next = Owner::new(
            [4; 16],
            StatusSnapshot {
                lifecycle: 1,
                installation: 1,
                reason: 0,
            },
        )
        .unwrap();
        next.restore(state, now, 1000).unwrap();
        assert_eq!(next.next_deadline(), Some(now + Duration::from_secs(1)));
        let write = next.take_write().unwrap();
        assert_eq!(write.state.proposals[0].reason, HoldReason::ClockUncertain);
    }
    #[test]
    fn live_rollback_holds_expired_debounce_and_recovers_without_ingress() {
        let now = Instant::now();
        let mut owner = owner(now);
        owner.ingest(activity(1, 1000, false), now, 1000).unwrap();
        owner.poll(now + Duration::from_secs(1), 999);
        assert!(owner.projects[&[1; 16]].clock_uncertain);
        assert_eq!(owner.next_deadline(), Some(now + Duration::from_secs(2)));
        owner.poll(now + Duration::from_secs(2), 999);
        assert_eq!(owner.next_deadline(), Some(now + Duration::from_secs(3)));
        assert!(owner.take_write().is_none());
        owner.poll(now + Duration::from_secs(3), 1000);
        assert!(!owner.projects[&[1; 16]].clock_uncertain);
        commit(&mut owner);
        assert_eq!(owner.snapshot([1; 16]).unwrap().proposals.len(), 1);
        assert!(owner.next_deadline().unwrap() > now + Duration::from_secs(3));
    }
    #[test]
    fn quiet_clock_samples_detect_rollback_and_rebase_proposal_expiry() {
        let now = Instant::now();
        let mut owner = owner(now);
        owner.ingest(activity(1, 1000, false), now, 1000).unwrap();
        owner.poll(now + Duration::from_millis(250), 1250);
        commit(&mut owner);
        owner.active_foreground([1; 16], 1).unwrap();
        owner.poll(now + Duration::from_secs(10), 201000);
        assert_eq!(
            owner.next_deadline(),
            Some(now + Duration::from_millis(110250))
        );
        owner.poll(now + Duration::from_secs(11), 200999);
        assert!(owner.projects[&[1; 16]].clock_uncertain);
        assert_eq!(owner.next_deadline(), Some(now + Duration::from_secs(12)));
        owner.poll(now + Duration::from_secs(12), 400000);
        commit(&mut owner);
        assert_eq!(
            owner.snapshot([1; 16]).unwrap().proposals[0].state,
            ProposalState::Expired
        );
        assert!(owner.next_deadline().is_none());
    }
    #[test]
    fn shutdown_flushes_debounced_evidence_without_new_ingress() {
        let now = Instant::now();
        let mut owner = owner(now);
        owner.ingest(activity(1, 1000, false), now, 1000).unwrap();
        owner.shutdown();
        assert!(!owner.settled());
        assert!(matches!(
            owner.ingest(activity(2, 1001, false), now, 1001),
            Err(Error::Unavailable)
        ));
        let write = owner.take_write().unwrap();
        assert_eq!(write.state.observations.len(), 1);
        assert_eq!(write.state.proposals.len(), 1);
        owner
            .persisted([1; 16], write.state.write_id, Ok(()))
            .unwrap();
        assert!(owner.settled());
        assert!(owner.next_deadline().is_none());
    }
    #[test]
    fn uncertain_write_reconciles_exact_identity_or_retries_same_write() {
        let now = Instant::now();
        let mut owner = owner(now);
        owner
            .capture_status([1; 16], [3; 16], 1, 1, now, 1000)
            .unwrap();
        let original = owner.snapshot([1; 16]).unwrap().clone();
        let write = owner.take_write().unwrap();
        assert_eq!(
            owner.persisted([1; 16], write.state.write_id, Err(Error::Unconfirmed)),
            Err(Error::Unconfirmed)
        );
        assert!(!owner.reconcile(original).unwrap());
        assert_eq!(owner.pending_write().unwrap().state, write.state);
        assert!(owner.reconcile(write.state.clone()).unwrap());
        assert_eq!(owner.snapshot([1; 16]).unwrap(), &write.state);
    }
    #[test]
    fn isolated_failure_does_not_block_other_project_persistence() {
        let now = Instant::now();
        let mut owner = owner(now);
        owner
            .capture_status([1; 16], [3; 16], 1, 1, now, 1000)
            .unwrap();
        let write = owner.take_write().unwrap();
        let _ = owner.persisted([1; 16], write.state.write_id, Err(Error::Unconfirmed));
        owner.disable([1; 16]);
        owner
            .restore(ProjectState::empty([9; 16]), now, 1000)
            .unwrap();
        owner
            .capture_status([9; 16], [4; 16], 1, 1, now, 1000)
            .unwrap();
        let next = owner.take_write().unwrap();
        assert_eq!(next.state.project, [9; 16]);
    }
    #[test]
    fn foreground_release_after_terminal_enables_idle_without_new_activity() {
        let now = Instant::now();
        let mut owner = owner(now);
        let mut terminal = activity(2, 1000, false);
        if let Payload::Activity {
            active_foreground, ..
        } = &mut terminal.payload
        {
            *active_foreground = 1;
        }
        owner.ingest(terminal, now, 1000).unwrap();
        owner.poll(now + Duration::from_millis(250), 1250);
        commit(&mut owner);
        // Worker settlement can release Active without changing journal sequence.
        owner.active_foreground([1; 16], 0).unwrap();
        owner.poll(now + IDLE, 301000);
        commit(&mut owner);
        assert!(
            owner
                .snapshot([1; 16])
                .unwrap()
                .proposals
                .iter()
                .any(|p| p.purpose == Purpose::Reflection)
        );
    }
}
