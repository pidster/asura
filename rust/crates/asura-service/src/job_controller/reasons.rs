//! Blocking and busy reasons with their brief texts (AQ1), and the mapping
//! of journal job codes to audit values. The journal codec owns the codes;
//! the audit schema mirrors them, and the exhaustive matches below keep the
//! two tables in step at compile time.
use asura_storage::audit as record;
use asura_storage::authority::conversation::{
    JobBlockReason, JobCaptureState, JobCleanup, JobOutcome, JobReason, JobRequester,
};

/// The fixed bound on unsettled jobs in the service: the checkpoint and
/// ledger bound (JR9).
pub(crate) const MAX_UNSETTLED: usize = 64;

/// The first failing start condition of a queue head, in condition order.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) enum BlockReason {
    Recovering,
    JournalUnavailable,
    JournalHeld,
    PayloadUnavailable,
    AgentSlots,
    ServiceSlots,
    PendingInstalls,
    Quota,
    DiskSpace,
}
impl BlockReason {
    pub(crate) fn audit(self) -> record::JobBlockReason {
        use record::JobBlockReason as A;
        match self {
            Self::Recovering => A::Recovering,
            Self::JournalUnavailable => A::JournalUnavailable,
            Self::JournalHeld => A::JournalHeld,
            Self::PayloadUnavailable => A::PayloadUnavailable,
            Self::AgentSlots => A::AgentSlots,
            Self::ServiceSlots => A::ServiceSlots,
            Self::PendingInstalls => A::PendingInstalls,
            Self::Quota => A::Quota,
            Self::DiskSpace => A::DiskSpace,
        }
    }
    /// The kind 29 state block reason (codes 1 to 9 in condition order).
    pub(crate) fn journal(self) -> JobBlockReason {
        match self {
            Self::Recovering => JobBlockReason::Recovering,
            Self::JournalUnavailable => JobBlockReason::JournalUnavailable,
            Self::JournalHeld => JobBlockReason::JournalHeld,
            Self::PayloadUnavailable => JobBlockReason::PayloadUnavailable,
            Self::AgentSlots => JobBlockReason::AgentSlots,
            Self::ServiceSlots => JobBlockReason::ServiceSlots,
            Self::PendingInstalls => JobBlockReason::PendingInstalls,
            Self::Quota => JobBlockReason::Quota,
            Self::DiskSpace => JobBlockReason::DiskSpace,
        }
    }
}

/// The live values that the brief texts of reasons 5 to 7 show.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Counts {
    pub agent_live: usize,
    pub agent_limit: usize,
    pub agent_awaiting_cleanup: usize,
    pub service_live: usize,
    pub service_limit: usize,
    pub service_awaiting_cleanup: usize,
    pub pending_installs: usize,
    /// Released `!!` jobs (OD2) among the live ones: shell exited, output
    /// still open. Shown in the slot texts only, never in kind 29.
    pub agent_released: usize,
    pub service_released: usize,
}

/// The brief text of a blocking reason with its live values.
pub(crate) fn block_text(reason: BlockReason, counts: &Counts) -> String {
    let cleanup = |n: usize, released: usize| match (n, released) {
        (0, 0) => String::new(),
        (n, 0) => format!(" ({n} awaiting cleanup)"),
        (0, r) => format!(" ({r} shell exited, output open)"),
        (n, r) => format!(" ({n} awaiting cleanup; {r} shell exited, output open)"),
    };
    match reason {
        BlockReason::Recovering => "waiting: recovering jobs after restart".into(),
        BlockReason::JournalUnavailable => "waiting: journal unavailable".into(),
        BlockReason::JournalHeld => "waiting: journal space is full".into(),
        BlockReason::PayloadUnavailable => "waiting: output store is recovering".into(),
        BlockReason::AgentSlots => format!(
            "waiting: {} of {} jobs running for this agent{}",
            counts.agent_live,
            counts.agent_limit,
            cleanup(counts.agent_awaiting_cleanup, counts.agent_released)
        ),
        BlockReason::ServiceSlots => format!(
            "waiting: {} of {} jobs running{}",
            counts.service_live,
            counts.service_limit,
            cleanup(counts.service_awaiting_cleanup, counts.service_released)
        ),
        BlockReason::PendingInstalls => format!(
            "waiting: {} finished jobs still saving output",
            counts.pending_installs
        ),
        BlockReason::Quota => "waiting: output quota full".into(),
        BlockReason::DiskSpace => "waiting: disk space low".into(),
    }
}

/// Why a submission was refused before any record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BusyReason {
    QueueFull,
    JobsFull,
    Closed,
}
pub(crate) fn audit_requester(requester: JobRequester) -> record::JobRequester {
    match requester {
        JobRequester::Model => record::JobRequester::Model,
        JobRequester::Client => record::JobRequester::Client,
        JobRequester::Person => record::JobRequester::Person,
        JobRequester::Service => record::JobRequester::Service,
    }
}

pub(crate) fn audit_reason(reason: JobReason) -> record::JobReason {
    use record::JobReason as A;
    match reason {
        JobReason::None => A::None,
        JobReason::ModelCancel => A::ModelCancel,
        JobReason::TurnEnded => A::TurnEnded,
        JobReason::ClientCancel => A::ClientCancel,
        JobReason::PersonStop => A::PersonStop,
        JobReason::PersonKill => A::PersonKill,
        JobReason::IdentityChanged => A::IdentityChanged,
        JobReason::Shutdown => A::Shutdown,
        JobReason::CaptureFault => A::CaptureFault,
        JobReason::Quota => A::Quota,
        JobReason::Enospc => A::Enospc,
        JobReason::Restart => A::Restart,
        JobReason::CwdUnavailable => A::CwdUnavailable,
        JobReason::ProjectChanged => A::ProjectChanged,
        JobReason::SandboxUnavailable => A::SandboxUnavailable,
        JobReason::SpawnFailed => A::SpawnFailed,
        JobReason::CaptureUnavailable => A::CaptureUnavailable,
        JobReason::ProtocolFault => A::ProtocolFault,
        JobReason::SetupFailed => A::SetupFailed,
        JobReason::ExecutionOff => A::ExecutionOff,
        JobReason::OutsideSandboxOff => A::OutsideSandboxOff,
    }
}

pub(crate) fn audit_outcome(outcome: JobOutcome) -> record::JobOutcome {
    match outcome {
        JobOutcome::Exited => record::JobOutcome::Exited,
        JobOutcome::Signalled => record::JobOutcome::Signalled,
        JobOutcome::Cancelled => record::JobOutcome::Cancelled,
        JobOutcome::NotStarted => record::JobOutcome::NotStarted,
        JobOutcome::Interrupted => record::JobOutcome::Interrupted,
    }
}

pub(crate) fn audit_cleanup(cleanup: JobCleanup) -> record::JobCleanup {
    match cleanup {
        JobCleanup::None => record::JobCleanup::None,
        JobCleanup::Confirmed => record::JobCleanup::Confirmed,
        JobCleanup::Late => record::JobCleanup::Late,
    }
}

pub(crate) fn audit_capture(state: JobCaptureState) -> record::JobCaptureState {
    match state {
        JobCaptureState::Complete => record::JobCaptureState::Complete,
        JobCaptureState::Incomplete => record::JobCaptureState::Incomplete,
        JobCaptureState::Unconfirmed => record::JobCaptureState::Unconfirmed,
        JobCaptureState::NotStarted => record::JobCaptureState::NotStarted,
        JobCaptureState::Interrupted => record::JobCaptureState::Interrupted,
    }
}

pub(crate) fn audit_exit_kind(kind: u8) -> record::JobExitKind {
    match kind {
        1 => record::JobExitKind::Exited,
        2 => record::JobExitKind::Signalled,
        _ => record::JobExitKind::Unknown,
    }
}

pub(crate) fn audit_unconfirmed(code: u8) -> record::JobUnconfirmedReason {
    match code {
        9 => record::JobUnconfirmedReason::CleanupFailed,
        13 => record::JobUnconfirmedReason::EscapeeUnterminated,
        _ => record::JobUnconfirmedReason::None,
    }
}
