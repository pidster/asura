//! The ports through which the job controller drives its collaborators
//! (tool-job-controller.md, slot worker, capture worker and controller
//! interfaces). Every method runs on the reactor thread and does bounded
//! in-memory work: no file, journal or process wait, and at most one
//! nonblocking 1-byte lease write (AE-01).
use super::Origin;
use crate::exec_cmd_worker::PersonLaunch;
use crate::{capture_worker, tools};
use asura_platform::{
    PollInterest, RuntimeDirectory,
    exec_cmd::{Exit, Profile, StopKind, UnconfirmedReason},
    secrets::{SecretCommand, SecretVariable},
};
use asura_storage::authority::conversation::{
    CaptureNumber, CaptureReason, CaptureState, Id, JobCleanup, JobReason, JobSettled, JobStream,
};
use asura_storage::payload::Victim;
use std::time::Instant;

/// The controller drives one slot per live job through this trait.
/// `exec_cmd_worker::Worker` implements it (J2-E); unit tests use the
/// fixture slot.
pub(crate) trait SlotPort {
    fn start(&mut self, start: SlotStart) -> Result<(), SlotStartError>;
    fn stop(&mut self, kind: StopKind); // Term or Kill; idempotent; never blocks
    fn discard_capture(&mut self); // cancel trigger in Draining: capture discards
    /// A trigger on a released `!!` job (after kind 2 reason 15, OD2): close
    /// the capture read ends and seal the stored bytes `incomplete` with
    /// capture reason 7. Called at most once; writes no lease byte and
    /// signals nothing. P3-T implements it in the exec_cmd slot.
    fn close_capture(&mut self);
    fn interests(&self, out: &mut Vec<PollInterest>);
    fn next_deadline(&self, now: Instant) -> Option<Instant>;
    fn poll(&mut self, now: Instant, out: &mut Vec<SlotEvent>); // at most 8 events
    fn idle(&self) -> bool;
}

/// Everything one slot needs to set up and run a job. The secret values move
/// into the guardian request; the capture start holds the job's 1 MiB
/// reservation and its expiry victims.
pub(crate) struct SlotStart {
    pub job: Id,
    pub profile: Profile,
    pub runtime: RuntimeDirectory,
    pub scope: tools::ExecutionScope,
    pub command: String,
    pub cwd: String,
    pub secrets: Vec<SecretVariable>,
    pub secret_command: Option<SecretCommand>,
    pub capture: capture_worker::Start,
    /// A person job's setup inputs, required for and matching a person
    /// profile, absent for `model-1` (`exec_cmd_worker::start_job` passes
    /// them to `start_job_with`).
    pub person: Option<PersonLaunch>,
}

/// Why a slot refused a start. The job settles `not_started` with no
/// process: `CaptureUnavailable` gives reason 16, the others reason 18.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SlotStartError {
    /// The slot already holds a job.
    Busy,
    /// The start does not name a runnable job.
    #[cfg_attr(test, allow(dead_code))] // the exec_cmd slot reports it
    Invalid,
    /// The capture worker could not start.
    CaptureUnavailable,
}

/// One slot report. A slot that accepted a start reports, in order:
/// `Spawned` or `SetupFailed` or `IdentityChanged`; then, after a spawn,
/// `LeaderExited`, at most one `CleanupUnconfirmed`, and `ProcessSettled`;
/// then exactly one `CaptureSettled`; then `ScratchCleaned` or
/// `ScratchRetained`. Pause events may come at any point before
/// `CaptureSettled`. A `model-1` job that spawned also reports at most one
/// `PathsChanged` after `ProcessSettled` and before its scratch event
/// (execution-path-guard.md, model change detection); it may come before
/// or after `CaptureSettled`.
pub(crate) enum SlotEvent {
    Spawned,                               // kind 1
    SetupFailed(JobReason),                // codes 12-18; no process
    LeaderExited(Exit),                    // kind 5
    CleanupUnconfirmed(UnconfirmedReason), // kind 4, or guardian lost
    CapturePaused(capture_worker::PauseReason),
    CaptureResumed,
    CaptureFault(CaptureReason),
    ProcessSettled {
        exit: Exit,
        cleanup: JobCleanup,
        escapees: u32,
        elapsed_ms: u64,
        /// `person-outside-1` kind 2 reason 15: a survivor holds a
        /// capture pipe (OD2); the slot maps it from the notice (P3-T).
        output_held: bool,
    }, // kind 2
    CaptureSettled(Box<CaptureOutcome>), // sealed report, or L3 unconfirmed
    ScratchCleaned,
    ScratchRetained,
    IdentityChanged, // pre-spawn recheck mismatch
    /// The build and test paths a model job changed (kind 32 action 1).
    PathsChanged(Box<crate::path_guard::ModelChange>),
}

/// The capture facts of one settled job, as kind 28 records them, and the
/// capture worker that outlives the slot. The slot builds them from the
/// worker's sealed report (with the per-stream line counts), or from the L3
/// bound as `unconfirmed`. `capture` is `None` when no worker ran.
pub(crate) struct CaptureOutcome {
    pub state: CaptureState,
    pub reason: CaptureReason,
    pub stdout: JobStream,
    pub stderr: JobStream,
    pub distinct_numbers: u32,
    pub numbers: Vec<CaptureNumber>,
    pub pause_count: u32,
    pub first_pause_ms: u64,
    pub paused_ms: u64,
    pub final_reservation: u64,
    pub capture: Option<capture_worker::Handle>,
}

/// Creates the slot of one granted job. The conversation owner supplies a
/// factory of `exec_cmd_worker::Worker`s registered with its wake.
pub(crate) type SlotFactory = Box<dyn FnMut() -> Box<dyn SlotPort>>;

/// Why a 1 MiB reservation failed after its expiry attempt (condition 8).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Short {
    Quota,
    DiskSpace,
}

/// The payload owner's job admission, as the controller uses it. The
/// conversation owner's `Payloads` implements it (J2-E).
pub(crate) trait PayloadPort {
    /// Condition 4: payload recovery finished and the store is open.
    fn ready(&self) -> bool;
    /// Condition 7: the length of the pending-install list (at most 4).
    fn pending_installs(&self) -> usize;
    /// A counter that grows on every ledger event that may free space:
    /// settlement, expiry, deletion. The controller re-evaluates queue
    /// heads when it changes; it never polls the ledger otherwise.
    fn space_epoch(&self) -> u64;
    /// Condition 8: reserves 1 MiB for the job after an expiry attempt and
    /// registers its payload IDs. `origin` owns the payloads.
    fn admit(&mut self, job: Id, origin: &Origin) -> Result<Admitted, Short>;
    /// A reservation whose job never reached a slot start (kind 26 failed
    /// or was withdrawn, a cancel before the start, or a slot start error):
    /// its expiry victims are restored and its reservation returns.
    fn release(&mut self, job: Id, victims: &[Victim]);
    /// Kind 28 committed at `sequence`: the ledger settles or releases the
    /// job, and the capture worker installs (R2) and joins the
    /// pending-install list, or is abandoned after `unconfirmed`.
    fn committed(
        &mut self,
        job: Id,
        settled: &JobSettled,
        sequence: u64,
        capture: Option<capture_worker::Handle>,
    );
    /// The journal became unavailable before kind 28 committed: no
    /// settlement; the worker is abandoned and its staging stays an
    /// unverified prefix for the next start's recovery.
    fn abandon(&mut self, job: Id, capture: Option<capture_worker::Handle>);
    /// The stored bytes of a live job's stdout and stderr: the readable
    /// watermark of each payload (J3 state block, `job wait` and
    /// `JobObserve` new-output ends). Arithmetic on the ledger; it never
    /// blocks. The controller reads it at each poll and keeps the maximum.
    /// The payload owner answers `Ledger::stored`, or zeros when the store
    /// is not ready or holds no entry.
    fn stored(&self, job: Id) -> [u64; 2];
    /// The disk-space recheck (TJ-Q9): measure free space again. Called
    /// at a paced recheck only while a queue head stays blocked by
    /// `disk_space`, once per recheck. Nonblocking: it sets the payload
    /// owner's one pending-measurement flag and wakes its maintenance
    /// worker; a request while a measurement is pending adds nothing. A
    /// changed effective quota reaches the controller as a `space_epoch`
    /// change. Before recovery, or after the store becomes unavailable, it
    /// does nothing.
    fn refresh_space(&mut self);
}

/// A granted reservation: the capture worker's store and the victims it
/// deletes before it creates staging.
pub(crate) struct Admitted {
    pub store: std::sync::Arc<asura_storage::payload::PayloadStore>,
    pub victims: Vec<Victim>,
}

/// Where the controller sends its audit events (`job.queued`,
/// `job.started`, `job.cancel_requested`, `job.settled`). The audit
/// emitter's admission never blocks; a dropped event is the emitter's gap.
pub(crate) trait AuditSink {
    fn emit(&self, event: asura_storage::audit::Event);
}
impl AuditSink for crate::audit::Emitter {
    fn emit(&self, event: asura_storage::audit::Event) {
        let _ = self.try_emit(event);
    }
}
