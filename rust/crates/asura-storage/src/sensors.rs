//! Typed sensor evidence; records confer no execution or disclosure authority.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
pub type Id = [u8; 16];
pub const MAX_OBSERVATIONS: usize = 64;
pub const MAX_PROPOSALS: usize = 8;
pub const MAX_REFERENCES: usize = 32;
pub const MAX_STATE_BYTES: usize = 128 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Limit,
    Conflict,
    Stale,
    Unavailable,
    Unconfirmed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Activity,
    Idle,
    ServiceStatus,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Accepted,
    Settled,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Payload {
    Activity {
        operation: Id,
        generation: u64,
        phase: Phase,
        active_foreground: u32,
    },
    Idle {
        since_ms: u64,
    },
    ServiceStatus {
        lifecycle: i32,
        installation: i32,
        reason: i32,
        operation: Id,
        generation: u64,
        ordinal: u32,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub id: Id,
    pub project: Id,
    pub source: Source,
    pub source_epoch: Id,
    pub sequence: u64,
    pub observed_ms: u64,
    pub received_ms: u64,
    pub expires_ms: u64,
    pub causal_root: Option<Id>,
    pub background: bool,
    pub causal_hops: u8,
    pub payload: Payload,
}
impl Observation {
    pub fn validate(&self) -> Result<(), Error> {
        if [self.id, self.project, self.source_epoch].contains(&[0; 16])
            || self.sequence == 0
            || self.observed_ms > self.received_ms
            || self.received_ms >= self.expires_ms
            || self.causal_root == Some([0; 16])
        {
            return Err(Error::Invalid);
        }
        let ttl = match (&self.source, &self.payload) {
            (
                Source::Activity,
                Payload::Activity {
                    operation,
                    generation,
                    ..
                },
            ) if *operation != [0; 16] && *generation > 0 => 300_000,
            (Source::Idle, Payload::Idle { since_ms }) if *since_ms <= self.observed_ms => 300_000,
            (
                Source::ServiceStatus,
                Payload::ServiceStatus {
                    operation,
                    generation,
                    ordinal,
                    lifecycle,
                    installation,
                    reason,
                },
            ) if *operation != [0; 16]
                && *generation > 0
                && *ordinal > 0
                && *ordinal <= 8
                && (0..=32).contains(lifecycle)
                && (0..=32).contains(installation)
                && (0..=128).contains(reason) =>
            {
                30_000
            }
            _ => return Err(Error::Invalid),
        };
        if self.received_ms - self.observed_ms > ttl
            || self.expires_ms - self.received_ms > ttl
            || self.id
                != observation_id(self.project, self.source, self.source_epoch, self.sequence)
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
pub fn observation_id(project: Id, source: Source, epoch: Id, sequence: u64) -> Id {
    let mut hash = Sha256::new();
    hash.update(b"asura-sensor-observation");
    hash.update(project);
    hash.update([match source {
        Source::Activity => 1,
        Source::Idle => 2,
        Source::ServiceStatus => 3,
    }]);
    hash.update(epoch);
    hash.update(sequence.to_be_bytes());
    let bytes = hash.finalize();
    bytes[..16].try_into().unwrap()
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Consolidation,
    Reflection,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    Held,
    Expired,
    Invalidated,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    SensorAdmissionUnavailable,
    ClockUncertain,
    ActivityResumed,
    Expired,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub id: Id,
    pub purpose: Purpose,
    pub state: ProposalState,
    pub reason: HoldReason,
    pub observations: Vec<Id>,
    pub omitted: u64,
    pub created_ms: u64,
    pub expires_ms: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectState {
    pub project: Id,
    pub revision: u64,
    pub write_id: Id,
    pub last_received_ms: u64,
    pub observations: Vec<Observation>,
    pub proposals: Vec<Proposal>,
}
impl ProjectState {
    pub fn empty(project: Id) -> Self {
        Self {
            project,
            revision: 0,
            write_id: [0; 16],
            last_received_ms: 0,
            observations: vec![],
            proposals: vec![],
        }
    }
    pub fn validate(&self) -> Result<(), Error> {
        if self.project == [0; 16] || (self.revision > 0 && self.write_id == [0; 16]) {
            return Err(Error::Invalid);
        }
        if self.observations.len() > MAX_OBSERVATIONS || self.proposals.len() > MAX_PROPOSALS {
            return Err(Error::Limit);
        }
        let mut ids = std::collections::HashSet::new();
        for value in &self.observations {
            value.validate()?;
            if value.project != self.project
                || !ids.insert(value.id)
                || value.received_ms > self.last_received_ms
            {
                return Err(Error::Invalid);
            }
        }
        let mut proposals = std::collections::HashSet::new();
        for value in &self.proposals {
            if value.id == [0; 16]
                || !proposals.insert(value.id)
                || value.created_ms >= value.expires_ms
                || value.expires_ms - value.created_ms > 300_000
                || value.observations.is_empty()
                || value.observations.len() > MAX_REFERENCES
                || value.observations.iter().any(|id| !ids.contains(id))
            {
                return Err(Error::Invalid);
            }
            if value
                .observations
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                != value.observations.len()
            {
                return Err(Error::Invalid);
            }
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<String, Error> {
        self.validate()?;
        let body = serde_yaml_ng::to_string(self).map_err(|_| Error::Invalid)?;
        if body.len() > MAX_STATE_BYTES {
            return Err(Error::Limit);
        }
        Ok(body)
    }
    pub fn decode(body: &str) -> Result<Self, Error> {
        if body.len() > MAX_STATE_BYTES {
            return Err(Error::Limit);
        }
        // The closed codec contains no strings requiring YAML anchors, aliases
        // or tags. Reject these before deserialization to prevent expansion.
        if body.bytes().any(|byte| matches!(byte, b'&' | b'*' | b'!')) {
            return Err(Error::Invalid);
        }
        let value: Self = serde_yaml_ng::from_str(body).map_err(|_| Error::Invalid)?;
        value.validate()?;
        Ok(value)
    }
    pub fn digest(&self) -> Result<[u8; 32], Error> {
        Ok(Sha256::digest(self.encode()?.as_bytes()).into())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn observation() -> Observation {
        Observation {
            id: observation_id([1; 16], Source::Activity, [2; 16], 1),
            project: [1; 16],
            source: Source::Activity,
            source_epoch: [2; 16],
            sequence: 1,
            observed_ms: 10,
            received_ms: 10,
            expires_ms: 300010,
            causal_root: None,
            background: false,
            causal_hops: 0,
            payload: Payload::Activity {
                operation: [3; 16],
                generation: 1,
                phase: Phase::Accepted,
                active_foreground: 1,
            },
        }
    }
    #[test]
    fn typed_round_trip_and_identity() {
        let mut value = ProjectState::empty([1; 16]);
        value.last_received_ms = 10;
        value.observations.push(observation());
        assert_eq!(
            ProjectState::decode(&value.encode().unwrap()),
            Ok(value.clone())
        );
        value.observations[0].project = [9; 16];
        assert_eq!(value.validate(), Err(Error::Invalid));
    }
    #[test]
    fn bounds_duplicate_and_wrong_source() {
        let mut value = ProjectState::empty([1; 16]);
        value.last_received_ms = 10;
        value.observations = vec![observation(); 65];
        assert_eq!(value.validate(), Err(Error::Limit));
        value.observations.truncate(2);
        assert_eq!(value.validate(), Err(Error::Invalid));
        let mut event = observation();
        event.source = Source::Idle;
        assert_eq!(event.validate(), Err(Error::Invalid));
        assert_eq!(
            ProjectState::decode(&"x".repeat(MAX_STATE_BYTES + 1)),
            Err(Error::Limit)
        );
    }
    #[test]
    fn proposal_requires_existing_unique_evidence() {
        let mut value = ProjectState::empty([1; 16]);
        value.last_received_ms = 10;
        value.observations.push(observation());
        value.proposals.push(Proposal {
            id: [4; 16],
            purpose: Purpose::Consolidation,
            state: ProposalState::Held,
            reason: HoldReason::SensorAdmissionUnavailable,
            observations: vec![[8; 16]],
            omitted: 0,
            created_ms: 10,
            expires_ms: 20,
        });
        assert_eq!(value.validate(), Err(Error::Invalid));
        value.proposals[0].observations = vec![observation().id];
        assert!(value.validate().is_ok());
    }
}
