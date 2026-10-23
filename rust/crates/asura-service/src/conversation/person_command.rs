//! Person commands (`!` and `!!`) in the conversation owner
//! (person-command-mode.md, packet P3-E; secret-store.md, SL5).
//!
//! The owner validates a `ConversationQueueCommandSubmit`, rewrites the
//! command with the running rule set (secret-detection.md, command
//! rewrite), stores the values in the input's Keychain item through the
//! secret store worker, then appends kind 31 on its serialized journal
//! ticket. At dispatch it reads the kind 31 back, reads the item, rebuilds
//! the secret variables and the person setup, and submits a person-origin
//! start to the job controller, which takes the slot, writes kind 26 and
//! runs the job. A `!!` job releases its lane on `JobEvent::ShellExited`
//! (OD2); every item is deleted after its deciding record.
//!
//! Every step here is bounded in-memory work on the reactor (AE-01): the
//! Keychain calls run on the secret store worker, the journal work on the
//! writer worker, the setup on the slot's setup thread. No wait has a time
//! bound: a submit ends by its reply, a dispatch read by its reply; a
//! locked Keychain is rechecked on the shared pacing, which ends nothing.
use super::{Body, Job, Owner, error, id, reply, storage_error};
use crate::exec_cmd_worker::PersonLaunch;
use crate::job_controller::{
    Admission, AgentKey, JobEvent, JobState, Origin, Pacing, Profile, Requester, StartRequest,
};
use crate::secret_store::{
    self, DeleteReply, EnvironmentEntry, GetReply, Polled, PutReply, Refused, SecretValue, Secrets,
    Store, StoreFailure, SweepReport, Ticket,
};
use crate::tools;
use asura_control::pb;
use asura_platform::keychain::Target;
use asura_platform::secrets::{
    self, JobRedaction, Plan, RefusalCode, RuleSet, SecretVariable, WHOLE_COMMAND,
};
use asura_storage::audit::{
    Event, PersonCommandRefusal as Refusal, SecretStoreReply as StoreReply,
};
use asura_storage::authority::conversation::{
    self as journal, ClientVariable, Id, InputKind, InputStatus, JobBlockReason, JobRunState,
    PersonCommandQueued,
};
use asura_storage::authority::writer;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;
use zeroize::Zeroizing;

/// Submits in flight at once (AE-03): twice the secret store queue, so a
/// full store queue answers `secret_store_busy` first. A submit beyond it
/// is `conversation_busy`.
const SUBMITS: usize = 2 * secret_store::QUEUE_BOUND;
/// Dropped inputs whose kind 31 waits to be read for the item decision.
const CHECKS: usize = 16;
/// The `!` PATH bounds (PC2).
const MAX_PATH_BYTES: usize = asura_control::person::MAX_SEARCH_PATH_BYTES;
const MAX_PATH_ENTRIES: usize = 64;
/// The |OSStatus| a refusal carries when this runtime has no Keychain
/// target or the store worker could not start: `errSecNotAvailable`
/// (-25291, "no keychain is available").
const NO_KEYCHAIN: u32 = 25_291;
/// The |OSStatus| of `errSecDuplicateItem` (-25299), reported when a fresh
/// input's item already exists with other values.
const DUPLICATE_ITEM: u32 = 25_299;

/// The outside switch: unknown until the start-time read.
const SWITCH_UNKNOWN: u8 = 0;
const SWITCH_OFF: u8 = 1;
const SWITCH_ON: u8 = 2;

/// Hold reasons of a kind 3 entry (control API additions, hold reasons).
pub(super) const SECRETS_LOST: &str = "secrets_lost";
pub(super) const SECRET_STORE_DENIED: &str = "secret_store_denied";
pub(super) const SECRET_RULES_CHANGED: &str = "secret_rules_changed";
/// A person waiter cancelled before its slot (a client `JobCancel`): the
/// input is held, so it never dispatches again without Resume.
pub(super) const CANCELLED: &str = "cancelled";

/// The secret store target for this runtime. Production uses the default
/// (login) keychain. A test-support build, or this crate's unit tests, use
/// the scratch keychain the harness names in `HOME/secret-store-keychain`
/// (its path, then its password, one per line), the default keychain only
/// for the account's own Asura home, and no target otherwise, so no test
/// reaches the login keychain (SSR6). Blocking (file read, account
/// database): the start-time read thread only.
pub(super) fn store_target(runtime: &asura_platform::RuntimeDirectory) -> Option<Target> {
    #[cfg(any(test, feature = "test-support"))]
    {
        let home = runtime.scratch_home_for_test()?;
        if let Ok(text) = std::fs::read_to_string(home.join("secret-store-keychain")) {
            let mut lines = text.lines();
            let path = std::path::PathBuf::from(lines.next()?);
            let password = Zeroizing::new(lines.next()?.to_owned());
            return Some(Target::Scratch { path, password });
        }
        let account = asura_platform::account_home_path().ok()?;
        (cfg!(not(test)) && home == account).then_some(Target::Default)
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        let _ = runtime;
        Some(Target::Default)
    }
}

/// The PC2 PATH sanitizer: at most 4,096 bytes and no NUL, empty and
/// relative entries dropped, order kept, at most 64 entries left. `None`
/// is `path_invalid`.
pub(super) fn sanitize_path(path: Option<&str>) -> Option<String> {
    let path = path?;
    if path.len() > MAX_PATH_BYTES || path.contains('\0') {
        return None;
    }
    let kept: Vec<&str> = path.split(':').filter(|e| e.starts_with('/')).collect();
    if kept.is_empty() || kept.len() > MAX_PATH_ENTRIES {
        return None;
    }
    Some(kept.join(":"))
}

/// The submit refusal of a `QueueRefusal` (exhaustive; audit names).
pub(super) fn queue_refusal(refusal: pb::QueueRefusal) -> Option<Refusal> {
    use pb::QueueRefusal as Q;
    Some(match refusal {
        Q::Unspecified => return None,
        Q::OutsideSandboxOff => Refusal::OutsideSandboxOff,
        Q::EnvironmentInvalid => Refusal::EnvironmentInvalid,
        Q::EnvironmentTooLarge => Refusal::EnvironmentTooLarge,
        Q::PathInvalid => Refusal::PathInvalid,
        Q::CwdInvalid => Refusal::CwdInvalid,
        Q::SecretStoreLocked => Refusal::SecretStoreLocked,
        Q::SecretStoreDenied => Refusal::SecretStoreDenied,
        Q::SecretStoreUnavailable => Refusal::SecretStoreUnavailable,
        Q::SecretStoreFull => Refusal::SecretStoreFull,
        Q::SecretStoreBusy => Refusal::SecretStoreBusy,
        Q::SecretInCwd => Refusal::SecretInCwd,
        Q::PlaceholderNotAvailable => Refusal::PlaceholderNotAvailable,
        Q::SecretDetectionUnavailable => Refusal::SecretDetectionUnavailable,
    })
}

/// The submit refusal of a writer error (exhaustive). The writer answers
/// `AlreadyInitialized` only to `Initialize` and `ModelUnavailable` only
/// to turn preparation; `QueuePersonCommand` never gets them, and the
/// audit enum has no member for them (reported to D0), so they are named
/// `invalid_request`, the token of a request the writer cannot take.
pub(super) fn error_refusal(error: writer::Error) -> Refusal {
    use writer::Error as E;
    match error {
        E::Busy => Refusal::ConversationBusy,
        E::NotInitialized => Refusal::InstallationNotInitialized,
        E::RepairRequired => Refusal::AuthorityRepairRequired,
        E::OutcomeUnconfirmed => Refusal::OutcomeUnconfirmed,
        E::Conflict => Refusal::RequestConflict,
        E::StaleProject => Refusal::ProjectStale,
        E::OverlapsAsuraState => Refusal::ProjectOverlapsAsuraState,
        E::Limit => Refusal::CapacityExhausted,
        E::Cancelled => Refusal::Cancelled,
        E::Invalid => Refusal::InvalidRequest,
        E::Unavailable => Refusal::StorageUnavailable,
        E::JournalQuota => Refusal::JournalQuota,
        E::JournalDiskSpace => Refusal::JournalDiskSpace,
        E::AlreadyInitialized | E::ModelUnavailable => Refusal::InvalidRequest,
    }
}

/// The `Error` token of a submit refusal that is not a `QueueRefusal`.
fn error_token(refusal: Refusal) -> &'static str {
    match refusal {
        Refusal::StaleGeneration => "stale_generation",
        Refusal::ConversationBusy => "conversation_busy",
        Refusal::RequestConflict => "request_conflict",
        Refusal::ServiceDraining => "service_draining",
        Refusal::ProjectUnknown => "project_unknown",
        Refusal::ProjectStale => "project_stale",
        Refusal::ProjectOverlapsAsuraState => "project_overlaps_asura_state",
        Refusal::InstallationNotInitialized => "installation_not_initialized",
        Refusal::AuthorityRepairRequired => "authority_repair_required",
        Refusal::OutcomeUnconfirmed => "outcome_unconfirmed",
        Refusal::CapacityExhausted => "capacity_exhausted",
        Refusal::Cancelled => "cancelled",
        Refusal::InvalidRequest => "invalid_request",
        Refusal::StorageUnavailable => "storage_unavailable",
        Refusal::JournalQuota => "journal_quota",
        Refusal::JournalDiskSpace => "journal_disk_space",
        // `QueueRefusal` names travel in the refusal field, not a token.
        _ => "invalid_request",
    }
}

/// The `QueueRefusal` of a person-command reason, or `None` for an
/// `Error` token.
fn typed_refusal(refusal: Refusal) -> Option<pb::QueueRefusal> {
    use pb::QueueRefusal as Q;
    Some(match refusal {
        Refusal::OutsideSandboxOff => Q::OutsideSandboxOff,
        Refusal::EnvironmentInvalid => Q::EnvironmentInvalid,
        Refusal::EnvironmentTooLarge => Q::EnvironmentTooLarge,
        Refusal::PathInvalid => Q::PathInvalid,
        Refusal::CwdInvalid => Q::CwdInvalid,
        Refusal::SecretStoreLocked => Q::SecretStoreLocked,
        Refusal::SecretStoreDenied => Q::SecretStoreDenied,
        Refusal::SecretStoreUnavailable => Q::SecretStoreUnavailable,
        Refusal::SecretStoreFull => Q::SecretStoreFull,
        Refusal::SecretStoreBusy => Q::SecretStoreBusy,
        Refusal::SecretInCwd => Q::SecretInCwd,
        Refusal::PlaceholderNotAvailable => Q::PlaceholderNotAvailable,
        Refusal::SecretDetectionUnavailable => Q::SecretDetectionUnavailable,
        _ => return None,
    })
}

/// The refusal of a store failure at submit.
fn store_refusal(failure: StoreFailure) -> (Refusal, Option<u32>) {
    match failure {
        StoreFailure::Locked => (Refusal::SecretStoreLocked, None),
        StoreFailure::Denied => (Refusal::SecretStoreDenied, None),
        StoreFailure::Unavailable { status, .. } => (
            Refusal::SecretStoreUnavailable,
            Some(status.unsigned_abs())
                .filter(|s| *s != 0)
                .or(Some(NO_KEYCHAIN)),
        ),
    }
}

/// The audit reply code and status of a store failure.
fn failure_reply(failure: &StoreFailure) -> (StoreReply, Option<u32>) {
    match failure {
        StoreFailure::Locked => (StoreReply::Locked, None),
        StoreFailure::Denied => (StoreReply::Denied, None),
        StoreFailure::Unavailable { status, .. } => {
            (StoreReply::Unavailable, Some(status.unsigned_abs()))
        }
    }
}
fn put_reply(reply: &PutReply) -> (StoreReply, Option<u32>) {
    match reply {
        PutReply::Stored => (StoreReply::Stored, None),
        PutReply::ExistsSame => (StoreReply::ExistsSame, None),
        PutReply::ExistsDifferent => (StoreReply::ExistsDifferent, None),
        PutReply::Full => (StoreReply::Full, None),
        PutReply::Failed(failure) => failure_reply(failure),
    }
}
fn get_reply(reply: &GetReply) -> (StoreReply, Option<u32>) {
    match reply {
        GetReply::Values(_) => (StoreReply::Read, None),
        GetReply::Missing => (StoreReply::Missing, None),
        GetReply::Mismatch => (StoreReply::Mismatch, None),
        GetReply::Failed(failure) => failure_reply(failure),
    }
}
fn delete_reply(reply: &DeleteReply) -> (StoreReply, Option<u32>) {
    match reply {
        DeleteReply::Deleted => (StoreReply::Deleted, None),
        DeleteReply::Absent => (StoreReply::Absent, None),
        DeleteReply::Failed(failure) => failure_reply(failure),
    }
}
/// The `secret_store.sweep` event of a sweep report.
fn sweep_event(report: &SweepReport) -> Event {
    let (reply, status) = match &report.outcome {
        Ok(()) => (StoreReply::Swept, None),
        Err(failure) => failure_reply(failure),
    };
    Event::SecretStoreSweep {
        reply,
        status,
        kept: report.kept,
        deleted: report.deleted,
        failed: report.failed,
        stranded: report.stranded,
        unrecognized: report.unrecognized,
        truncated: report.truncated,
        listings: report.listings,
    }
}

/// A submit that refuses: the reason and, for `secret_store_unavailable`,
/// the |OSStatus|.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Refusing {
    pub reason: Refusal,
    pub status: Option<u32>,
}
impl From<Refusal> for Refusing {
    fn from(reason: Refusal) -> Self {
        let status = (reason == Refusal::SecretStoreUnavailable).then_some(NO_KEYCHAIN);
        Self { reason, status }
    }
}

/// Where one submit is.
enum Stage {
    /// Waiting for the start-time read (the outside switch) or the rule
    /// set; re-planned when it is ready.
    Waiting,
    /// Planned: the request ID is looked up in the journal on the journal
    /// ticket before any Keychain call (ordering rule 6, P3-E2). The
    /// values wait here, zeroed on drop.
    Lookup(Box<Secrets>),
    /// The item `put` is in flight (ordering rule 1).
    Put(Ticket<PutReply>),
    /// An exact retry: the original item is read to compare the values
    /// (ordering rule 6).
    Retry(Ticket<GetReply>, Box<Secrets>),
    /// Ready for the journal ticket.
    Append,
}

/// One person command submit from validation to its reply.
pub(super) struct Submit {
    request: pb::Envelope,
    record: Box<PersonCommandQueued>,
    /// A Keychain item was stored for this submit's input.
    stored: bool,
    /// The item holds values (`!!`, or a command secret).
    needs_item: bool,
    /// An exact retry of a recorded request: the writer answers with the
    /// original input and nothing is appended.
    retry: bool,
    stage: Stage,
}

/// Why a dispatch waits for the Keychain at the lane head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum KeychainWait {
    Locked,
    Unavailable,
    Busy,
}

/// Where one dispatched person command is, until its kind 26 commits (or
/// it is held).
enum Dispatch {
    /// The kind 31 read is queued for, or holds, the journal ticket.
    Reading,
    /// The kind 31 is read; the item read or the controller submit follows.
    Ready(Box<PersonCommandQueued>),
    /// The item read is in flight; a recheck keeps its pacing.
    Getting(Box<PersonCommandQueued>, Ticket<GetReply>, Option<Pacing>),
    /// The Keychain is locked, unavailable or busy: a paced recheck.
    Waiting(Box<PersonCommandQueued>, KeychainWait, Pacing),
    /// The values are read; the start waits for the rule set, the
    /// project's registration or room in the controller's known-size
    /// queues (AQ1), and is tried again on the next wake. The values stay
    /// in this dispatch only, zeroed on drop.
    Admit(Box<PersonCommandQueued>, Option<Box<Secrets>>),
    /// The controller holds the start: a waiter, then a job.
    Submitted(Id),
    /// A hold decision is queued for the journal ticket, or a kind 31 read
    /// holds it.
    Holding,
}

/// The startup sweep (SS-F1): after replay and start-time recovery.
#[derive(Default)]
enum Sweep {
    #[default]
    NotStarted,
    Running(Ticket<SweepReport>),
    Done,
}

/// The person command state of the conversation owner.
pub(super) struct Persons {
    /// The secret store, once the installation and the target are known.
    store: Option<Store>,
    /// The start-time target: `None` until read; `Some(None)` when this
    /// runtime has none.
    target: Option<Option<Target>>,
    /// The store worker could not start: every item call is unavailable.
    store_failed: bool,
    /// The outside switch (`personCommands.allowOutsideSandbox`), shared
    /// with the controller's slot-grant check.
    switch: Arc<AtomicU8>,
    submits: Vec<Submit>,
    dispatches: BTreeMap<Id, Dispatch>,
    /// Inputs of released `!!` jobs (OD2), in memory only.
    released: BTreeSet<Id>,
    /// Inputs known to have an item: stored here, or read at dispatch.
    items: BTreeSet<Id>,
    /// Deletes in flight.
    deletes: Vec<(Id, Ticket<DeleteReply>)>,
    /// Dropped inputs whose kind 31 must be read to know whether an item
    /// exists.
    checks: VecDeque<Id>,
    /// A check read holds the journal ticket.
    checking: Option<Id>,
    sweep: Sweep,
    /// The start-time unsettled jobs: the sweep waits for their
    /// `interrupted` settlements.
    recovering: Option<BTreeSet<Id>>,
    /// Hold decisions waiting for the journal ticket.
    holds: VecDeque<Id>,
    /// The waiting reasons last published, by input (live display state).
    published: BTreeMap<Id, (i32, String)>,
    /// Test-support seam (SS-I3): kind 31 appends are held for this run.
    #[cfg(feature = "test-support")]
    pub(super) hold_append: bool,
    /// Owner unit tests: every delete the owner decided, in order.
    #[cfg(test)]
    deleted: Vec<Id>,
}
impl Default for Persons {
    fn default() -> Self {
        Self {
            store: None,
            target: None,
            store_failed: false,
            switch: Arc::new(AtomicU8::new(SWITCH_UNKNOWN)),
            submits: Vec::new(),
            dispatches: BTreeMap::new(),
            released: BTreeSet::new(),
            items: BTreeSet::new(),
            deletes: Vec::new(),
            checks: VecDeque::new(),
            checking: None,
            sweep: Sweep::NotStarted,
            recovering: None,
            holds: VecDeque::new(),
            published: BTreeMap::new(),
            #[cfg(feature = "test-support")]
            hold_append: false,
            #[cfg(test)]
            deleted: Vec::new(),
        }
    }
}

impl Persons {
    /// The inputs of released `!!` jobs, for lane readiness.
    pub(super) fn released(&self) -> &BTreeSet<Id> {
        &self.released
    }
    /// A persisted switch change, or the start-time value.
    pub(super) fn set_switch(&self, on: bool) {
        self.switch
            .store(if on { SWITCH_ON } else { SWITCH_OFF }, Ordering::Release);
    }
    fn switch(&self) -> Option<bool> {
        match self.switch.load(Ordering::Acquire) {
            SWITCH_OFF => Some(false),
            SWITCH_ON => Some(true),
            _ => None,
        }
    }
    /// The facts for `ServiceObservation` field 8: absent until the store
    /// exists.
    pub(super) fn observation(&self) -> Option<pb::SecretStoreStatus> {
        let o = self.store.as_ref()?.observation();
        Some(pb::SecretStoreStatus {
            item_count: o.item_count,
            sweep_pending: Some(o.sweep_pending),
            last_status: Some(o.last_status),
            stranded_count: Some(o.stranded_count),
            unrecognized_count: Some(o.unrecognized_count),
            stranded_inputs: o.stranded_inputs.iter().map(|i| i.to_vec()).collect(),
        })
    }
    /// The next paced Keychain recheck.
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.dispatches
            .values()
            .filter_map(|d| match d {
                Dispatch::Waiting(_, _, pacing) => Some(pacing.at()),
                _ => None,
            })
            .min()
    }
    /// The person's next action in a conversation (Send now, Resume, a new
    /// submit, Drop) rechecks its Keychain waits at once; the pacing starts
    /// again from its first delay.
    fn recheck_now(&mut self, inputs: &BTreeSet<Id>) {
        for (input, dispatch) in &mut self.dispatches {
            if inputs.contains(input)
                && matches!(dispatch, Dispatch::Waiting(..))
                && let Dispatch::Waiting(record, ..) =
                    std::mem::replace(dispatch, Dispatch::Reading)
            {
                *dispatch = Dispatch::Ready(record);
            }
        }
    }
    /// Shutdown: queued submits are answered `service_draining` by the
    /// owner; nothing new starts.
    pub(super) fn has_journal_work(&self) -> bool {
        !self.holds.is_empty()
            || self
                .submits
                .iter()
                .any(|s| matches!(s.stage, Stage::Lookup(_) | Stage::Append))
            || (self.checking.is_none() && !self.checks.is_empty())
            || self
                .dispatches
                .values()
                .any(|d| matches!(d, Dispatch::Reading))
    }
}

/// The secret variables of a stored rewrite: kind 31's command with each
/// `"$ASURA_SECRET_<n>"` or `${ASURA_SECRET_<n>}` replaced by its value
/// gives the raw command back, because the rewrite only inserted those
/// forms and a raw command never names them (`placeholder_not_available`).
/// The raw text is re-planned with the running rule set; the plan must be
/// the same rewrite with the same numbers and kinds, or the item does not
/// belong to this record (`mismatch`).
fn replan_rewrite(
    rules: &'static RuleSet,
    record: &PersonCommandQueued,
    values: &[SecretValue],
) -> Option<Vec<SecretVariable>> {
    let raw = restore(&record.command, values)?;
    let plan = secrets::rewrite_exec_cmd_call_with(rules, &raw, &record.cwd).ok()?;
    let Plan::Rewritten(rewrite) = plan else {
        return None;
    };
    let same = rewrite.command == record.command
        && rewrite.variables.len() == values.len()
        && rewrite.variables.len() == record.number_kinds.len()
        && rewrite
            .variables
            .iter()
            .zip(values)
            .zip(&record.number_kinds)
            .all(|((v, stored), kind)| {
                v.number() == u16::from(stored.number)
                    && v.value() == stored.value.as_slice()
                    && v.kind() == *kind
            });
    same.then_some(rewrite.variables)
}

/// The raw command of a rewrite: each inserted variable form replaced by
/// its stored value. `None` when a form names no stored number.
fn restore(command: &str, values: &[SecretValue]) -> Option<Zeroizing<String>> {
    let bytes = command.as_bytes();
    let mut out = Zeroizing::new(Vec::with_capacity(command.len()));
    let mut at = 0;
    while at < bytes.len() {
        let rest = &bytes[at..];
        let form = if rest.starts_with(b"\"$ASURA_SECRET_") {
            Some((15usize, b'"'))
        } else if rest.starts_with(b"${ASURA_SECRET_") {
            Some((15usize, b'}'))
        } else {
            None
        };
        if let Some((prefix, close)) = form {
            let digits = rest[prefix..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
            if digits > 0 && rest.get(prefix + digits) == Some(&close) {
                let number: u8 = std::str::from_utf8(&rest[prefix..prefix + digits])
                    .ok()?
                    .parse()
                    .ok()?;
                let value = values.iter().find(|v| v.number == number)?;
                out.extend_from_slice(&value.value);
                at += prefix + digits + 1;
                continue;
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    let text = String::from_utf8(std::mem::take(&mut *out)).ok()?;
    Some(Zeroizing::new(text))
}

/// The display-only copy of a whole-command input: cut at 8,192 bytes on a
/// character boundary, never inside a placeholder (kind 31 field 9, bit 0),
/// by the platform's one command cut.
fn cut_display(display: &str) -> (String, bool) {
    let mut text = display.to_owned();
    let placeholders = secrets::placeholder_ranges(&text);
    let cut = secrets::cut_command(&mut text, &placeholders);
    (text, cut)
}

/// The kind 31 secret facts of a plan: fields 8, 9, 14 to 17 and 20 to 22,
/// and the values for the item.
struct Planned {
    command: String,
    display: String,
    number_kinds: Vec<asura_platform::secrets::SecretKind>,
    unnumbered: u32,
    cut: bool,
    whole: bool,
    fallback: Option<journal::ExecCmdRefusal>,
    secrets: Secrets,
}
fn planned(plan: Plan) -> Planned {
    match plan {
        Plan::Rewritten(rewrite) => {
            let tokens = secrets::placeholder_tokens(&rewrite.command);
            Planned {
                number_kinds: rewrite.variables.iter().map(SecretVariable::kind).collect(),
                unnumbered: tokens.zero,
                cut: false,
                whole: false,
                fallback: None,
                secrets: Secrets {
                    variables: rewrite
                        .variables
                        .iter()
                        .map(|v| SecretValue {
                            number: v.number() as u8,
                            value: Zeroizing::new(v.value().to_vec()),
                        })
                        .collect(),
                    whole_command: None,
                    launch_environment: None,
                },
                display: String::new(),
                command: rewrite.command,
            }
        }
        Plan::WholeCommand(whole) => {
            let (display, cut) = cut_display(&whole.display);
            let tokens = secrets::placeholder_tokens(&display);
            let shown = tokens.max as usize;
            Planned {
                number_kinds: whole
                    .variables
                    .iter()
                    .take(shown)
                    .map(SecretVariable::kind)
                    .collect(),
                unnumbered: tokens.zero,
                cut,
                whole: true,
                fallback: Some(super::exec_cmd_call::refusal(&whole.reason)),
                secrets: Secrets {
                    variables: Vec::new(),
                    whole_command: Some(Zeroizing::new(whole.command.value().to_vec())),
                    launch_environment: None,
                },
                display,
                command: WHOLE_COMMAND.into(),
            }
        }
    }
}

/// The client variables of a `!` submit in kind 31 name-code order.
fn client_variables(v: &pb::ConversationQueueCommandSubmit) -> Vec<(ClientVariable, String)> {
    let mut out: Vec<(ClientVariable, String)> = v
        .environment
        .iter()
        .filter_map(|e| {
            let name = e.name.as_deref()?;
            let code = asura_control::person::CLIENT_VARIABLES
                .iter()
                .position(|n| *n == name)?;
            let variable = ClientVariable::from_code(u8::try_from(code + 1).ok()?)?;
            Some((variable, e.value.clone()?))
        })
        .collect();
    out.sort_by_key(|(variable, _)| *variable as u8);
    out
}

/// The kind 3 queue entry of a person command: its kind 31 lane facts, the
/// command as recorded, the cwd, the outside flag, the filtered mark and
/// the waiting reason of a Queued lane head.
pub(super) fn queue_entry(
    view: &writer::InputView,
    person: &PersonCommandQueued,
    full: bool,
    state: InputStatus,
    hold_reason: Option<&str>,
    waiting: Option<&(i32, String)>,
) -> pb::ConversationQueueEntry {
    let mut text = person.command_text().to_owned();
    if !full && text.len() > 256 {
        let mut end = 256;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    let waiting = waiting.filter(|_| state == InputStatus::Queued);
    pb::ConversationQueueEntry {
        command_cwd: Some(person.cwd.clone()),
        command_outside_sandbox: Some(person.outside_sandbox),
        waiting_reason: waiting.map(|(reason, _)| *reason),
        waiting_text: waiting.map(|(_, text)| text.clone()),
        command_filtered: person.whole_command.then_some(true),
        input_id: Some(view.input.input.to_vec()),
        project_id: Some(view.input.project.to_vec()),
        conversation_id: Some(view.input.conversation.to_vec()),
        target_operation_id: None,
        target_generation: Some(view.target_generation),
        kind: Some(InputKind::Person as u32),
        state: Some(state as u32),
        text: Some(text),
        sequence: Some(view.sequence),
        operation_id: None,
        generation: None,
        new_conversation: Some(view.input.new_conversation),
        order_position: Some(view.order_position),
        hold_reason: (state == InputStatus::Held)
            .then_some(hold_reason)
            .flatten()
            .map(str::to_owned),
        waiting_step: None,
    }
}

/// The waiting text of a Keychain wait (control API additions).
fn keychain_text(wait: KeychainWait) -> (i32, &'static str) {
    match wait {
        KeychainWait::Locked => (
            pb::QueueWaitReason::KeychainLocked as i32,
            "waiting: Keychain locked",
        ),
        KeychainWait::Unavailable => (
            pb::QueueWaitReason::KeychainUnavailable as i32,
            "waiting: Keychain unavailable",
        ),
        KeychainWait::Busy => (
            pb::QueueWaitReason::KeychainBusy as i32,
            "waiting: Keychain requests queued",
        ),
    }
}
/// A queued person job's controller block reason as a wait reason (1-9
/// equal the J2 block reasons).
fn block_reason(reason: JobBlockReason) -> i32 {
    reason as i32
}

impl Owner {
    // ----- Submit -----

    /// One `ConversationQueueCommandSubmit` (control API additions). It
    /// waits in the foreground FIFO like a kind 18 submit; once dispatched
    /// it is validated, planned and, with values, stored in the Keychain
    /// before kind 31 is appended (ordering rule 1). Every refusal names a
    /// `PersonCommandRefusal` and is audited.
    pub(super) fn person_submit(&mut self, request: pb::Envelope) -> Option<pb::Envelope> {
        if self.closing {
            return Some(self.person_refuse(request, Refusal::ServiceDraining.into()));
        }
        if !self.started || matches!(self.pending, Some((_, Job::Open | Job::AutoInitialize))) {
            return Some(self.person_refuse(request, Refusal::ConversationBusy.into()));
        }
        if let Some(reason) = self.unavailable {
            let refusal = Refusal::from_name(reason).unwrap_or(Refusal::StorageUnavailable);
            return Some(self.person_refuse(request, refusal.into()));
        }
        if !self.dispatching_foreground && (self.pending.is_some() || !self.foreground.is_empty()) {
            if self.foreground.len() >= 32 {
                return Some(self.person_refuse(request, Refusal::ConversationBusy.into()));
            }
            self.foreground.push_back(request);
            return None;
        }
        let Some(Body::ConversationQueueCommandSubmit(value)) = request.body.as_ref() else {
            return Some(error(request, "invalid_request"));
        };
        let request_id = id(&value.request_id);
        if self.persons.submits.len() >= SUBMITS
            || self
                .persons
                .submits
                .iter()
                .any(|s| s.record.request == request_id)
        {
            return Some(self.person_refuse(request, Refusal::ConversationBusy.into()));
        }
        // The person's next action in this conversation rechecks a
        // Keychain wait at once.
        if let Some(conversation) = value.conversation_id.as_deref() {
            self.recheck_conversation(conversation);
        }
        let mut submit = Submit {
            record: Box::new(PersonCommandQueued {
                request: request_id,
                digest: [0; 32],
                input: [0; 16],
                project: id(&value.project_id),
                conversation: value
                    .conversation_id
                    .as_deref()
                    .and_then(|c| c.try_into().ok())
                    .unwrap_or([0; 16]),
                expected_generation: value.expected_generation.unwrap_or(0),
                new_conversation: value.new_conversation == Some(true),
                outside_sandbox: value.outside_sandbox == Some(true),
                command: String::new(),
                display_command: String::new(),
                cwd: value.cwd.clone().unwrap_or_default(),
                search_path: String::new(),
                environment: Vec::new(),
                order_revision: 0,
                number_kinds: Vec::new(),
                unnumbered: 0,
                command_cut: false,
                whole_command: false,
                rule_set_revision: 0,
                rule_set_digest: [0; 32],
                fallback: None,
            }),
            request,
            stored: false,
            needs_item: false,
            retry: false,
            stage: Stage::Waiting,
        };
        match self.person_plan(&mut submit) {
            Ok(()) => {
                self.persons.submits.push(submit);
                None
            }
            Err(refused) => Some(self.person_refuse(submit.request, refused)),
        }
    }

    /// Validates and plans a waiting submit. `Ok` with `Stage::Waiting`
    /// waits for the switch or the rule set; otherwise the submit has its
    /// item request in flight or is ready to append.
    fn person_plan(&mut self, submit: &mut Submit) -> Result<(), Refusing> {
        let Some(Body::ConversationQueueCommandSubmit(value)) = submit.request.body.as_ref() else {
            return Err(Refusal::InvalidRequest.into());
        };
        // Form checks first: the shared environment owner, then the cwd
        // form and the PATH (P3-C codec rules, PCR9, PC2).
        if let Some(refusal) = asura_control::person::environment_refusal(value) {
            return Err(queue_refusal(refusal)
                .unwrap_or(Refusal::EnvironmentInvalid)
                .into());
        }
        let outside = value.outside_sandbox == Some(true);
        let cwd = value.cwd.as_deref().unwrap_or_default();
        if !asura_control::person::cwd_form_valid(cwd) {
            return Err(Refusal::CwdInvalid.into());
        }
        let search_path = if outside {
            String::new()
        } else {
            sanitize_path(value.search_path.as_deref())
                .ok_or(Refusing::from(Refusal::PathInvalid))?
        };
        // The outside switch at submit (switch check 1).
        if outside {
            match self.persons.switch() {
                None => return Ok(()),
                Some(false) => return Err(Refusal::OutsideSandboxOff.into()),
                Some(true) => {}
            }
        }
        // The rule set: a reactor user waits for the warm wake (SD-S2).
        let rules = match self.rules.need(self.wake.as_ref()) {
            None => return Ok(()),
            Some(Err(_)) => return Err(Refusal::SecretDetectionUnavailable.into()),
            Some(Ok(rules)) => rules,
        };
        let command = value.command.as_deref().unwrap_or_default();
        let plan = match secrets::rewrite_exec_cmd_call_with(rules, command, cwd) {
            Ok(plan) => plan,
            Err(refusal) => {
                return Err(match refusal.code {
                    RefusalCode::PlaceholderNotAvailable => Refusal::PlaceholderNotAvailable,
                    RefusalCode::SecretInCwd => Refusal::SecretInCwd,
                    // Detection unavailable and, defensively, any code the
                    // planner does not keep.
                    _ => Refusal::SecretDetectionUnavailable,
                }
                .into());
            }
        };
        let planned = planned(plan);
        let record = &mut submit.record;
        record.command = planned.command;
        record.display_command = planned.display;
        record.number_kinds = planned.number_kinds;
        record.unnumbered = planned.unnumbered;
        record.command_cut = planned.cut;
        record.whole_command = planned.whole;
        record.fallback = planned.fallback;
        record.rule_set_revision = rules.revision();
        record.rule_set_digest = rules.digest();
        record.search_path = search_path;
        record.environment = if outside {
            Vec::new()
        } else {
            client_variables(value)
        };
        let mut values = planned.secrets;
        if outside {
            values.launch_environment = Some(
                value
                    .launch_environment
                    .iter()
                    .map(|e| EnvironmentEntry {
                        name: Zeroizing::new(e.name.clone().unwrap_or_default()),
                        value: Zeroizing::new(e.value.clone().unwrap_or_default()),
                    })
                    .collect(),
            );
        }
        submit.needs_item =
            outside || !values.variables.is_empty() || values.whole_command.is_some();
        record.digest =
            journal::request_digest(journal::Request::QueuePersonCommand(record.request()))
                .map_err(|_| Refusing::from(Refusal::InvalidRequest))?;
        if self.replay.is_none() {
            return Err(Refusal::InstallationNotInitialized.into());
        }
        submit.stage = Stage::Lookup(Box::new(values));
        Ok(())
    }

    /// The journal lookup of a planned submit's request ID (ordering rule
    /// 6): a recorded request, including one older than the replay
    /// checkpoint, is matched by its kind 31 digest and, when its item
    /// exists, its values; it never writes a new item. Otherwise a fresh
    /// input is chosen and its item put. A new conversation waits for no
    /// pending one: the project holds one at a time (PR1), so the submit
    /// is `conversation_busy` while an unresolved input of the project
    /// waits in a generation-0 conversation.
    pub(super) fn person_looked_up(&mut self, mut submit: Submit, value: writer::Reply) {
        let result = match std::mem::replace(&mut submit.stage, Stage::Waiting) {
            Stage::Lookup(values) => {
                self.person_decide(&mut submit, *values, value.request_outcome)
            }
            // Only a lookup reaches here.
            _ => Err(Refusal::InvalidRequest.into()),
        };
        match result {
            Ok(()) => self.persons.submits.push(submit),
            Err(refused) => self
                .out
                .push_back(self.person_refuse(submit.request, refused)),
        }
    }

    fn person_decide(
        &mut self,
        submit: &mut Submit,
        values: Secrets,
        prior: Option<journal::RequestResult>,
    ) -> Result<(), Refusing> {
        if self.closing {
            return Err(Refusal::ServiceDraining.into());
        }
        let Some(state) = self.replay.clone() else {
            return Err(Refusal::InstallationNotInitialized.into());
        };
        let record = &mut submit.record;
        if let Some(prior) = prior {
            let journal::Outcome::QueueInput {
                input,
                conversation,
                ..
            } = prior.result
            else {
                return Err(Refusal::RequestConflict.into());
            };
            if prior.kind != 31 || prior.digest != record.digest {
                return Err(Refusal::RequestConflict.into());
            }
            submit.retry = true;
            record.input = input;
            record.conversation = conversation;
            if submit.needs_item
                && let Some(store) = &self.persons.store
            {
                let binding = secret_store::binding(
                    &state.installation_id,
                    &record.project,
                    &conversation,
                    &input,
                    &record.digest,
                );
                if let Ok(ticket) = store.get(input, binding) {
                    submit.stage = Stage::Retry(ticket, Box::new(values));
                    return Ok(());
                }
            }
            submit.stage = Stage::Append;
            return Ok(());
        }
        if record.new_conversation
            && state.inputs.values().any(|q| {
                q.project == record.project
                    && state
                        .conversations
                        .get(&q.conversation)
                        .is_some_and(|c| c.generation == 0)
                    && matches!(
                        state.input_status(q.request),
                        Some(InputStatus::Queued | InputStatus::Held)
                    )
            })
        {
            return Err(Refusal::ConversationBusy.into());
        }
        record.input = asura_platform::random_id();
        if record.new_conversation {
            record.conversation = asura_platform::random_id();
        }
        if !submit.needs_item {
            submit.stage = Stage::Append;
            return Ok(());
        }
        let Some(store) = &self.persons.store else {
            return Err(Refusal::SecretStoreUnavailable.into());
        };
        let binding = secret_store::binding(
            &state.installation_id,
            &record.project,
            &record.conversation,
            &record.input,
            &record.digest,
        );
        match store.put(record.input, binding, values) {
            Ok(ticket) => {
                submit.stage = Stage::Put(ticket);
                Ok(())
            }
            Err(Refused::Busy) => Err(Refusal::SecretStoreBusy.into()),
            Err(Refused::Stopped) => Err(Refusal::SecretStoreUnavailable.into()),
        }
    }

    /// A refusal reply, audited as `person_command.refused`. A
    /// `QueueRefusal` travels in the reply's refusal field; any other
    /// reason is its `Error` token.
    pub(super) fn person_refuse(&self, request: pb::Envelope, refused: Refusing) -> pb::Envelope {
        if let Some(Body::ConversationQueueCommandSubmit(value)) = request.body.as_ref()
            && let (Ok(project), Ok(request_id)) = (
                <[u8; 16]>::try_from(value.project_id.as_deref().unwrap_or_default()),
                <[u8; 16]>::try_from(value.request_id.as_deref().unwrap_or_default()),
            )
            && let Some(emitter) = &self.audit_emitter
        {
            let _ = emitter.try_emit(Event::PersonCommandRefused {
                project,
                request: request_id,
                outside_sandbox: value.outside_sandbox == Some(true),
                reason: refused.reason,
            });
        }
        match typed_refusal(refused.reason) {
            Some(refusal) => {
                let request_id = match request.body.as_ref() {
                    Some(Body::ConversationQueueCommandSubmit(v)) => v.request_id.clone(),
                    _ => None,
                };
                reply(
                    request,
                    Body::ConversationQueueReply(pb::ConversationQueueReply {
                        entries: Vec::new(),
                        request_id,
                        full_text: Some(false),
                        revision: self.replay.as_ref().map(|r| r.revision),
                        pending: Some(false),
                        order_revision: None,
                        stale_order: Some(false),
                        accepted_input_id: None,
                        refusal: Some(refusal as i32),
                        refusal_status: if refusal == pb::QueueRefusal::SecretStoreUnavailable {
                            Some(refused.status.unwrap_or(NO_KEYCHAIN))
                        } else {
                            None
                        },
                    }),
                )
            }
            None => error(request, error_token(refused.reason)),
        }
    }

    /// Advances the submits: waiting plans, item replies and retry reads.
    fn poll_submits(&mut self) {
        let mut submits = std::mem::take(&mut self.persons.submits);
        let mut kept = Vec::with_capacity(submits.len());
        for mut submit in submits.drain(..) {
            if self.closing {
                self.out
                    .push_back(self.person_refuse(submit.request, Refusal::ServiceDraining.into()));
                continue;
            }
            let result = match std::mem::replace(&mut submit.stage, Stage::Waiting) {
                Stage::Waiting => self.person_plan(&mut submit),
                Stage::Put(ticket) => match ticket.poll() {
                    Polled::Pending => {
                        submit.stage = Stage::Put(ticket);
                        Ok(())
                    }
                    Polled::Ready(reply) => {
                        let (code, status) = put_reply(&reply);
                        self.audit_store(Event::SecretStorePut {
                            input: submit.record.input,
                            reply: code,
                            status,
                        });
                        self.person_put(&mut submit, reply)
                    }
                    Polled::Lost => Err(Refusal::SecretStoreUnavailable.into()),
                },
                Stage::Retry(ticket, values) => match ticket.poll() {
                    Polled::Pending => {
                        submit.stage = Stage::Retry(ticket, values);
                        Ok(())
                    }
                    Polled::Ready(reply) => {
                        let (code, status) = get_reply(&reply);
                        self.audit_store(Event::SecretStoreGet {
                            input: submit.record.input,
                            reply: code,
                            status,
                        });
                        match reply {
                            // Equal values return the original input;
                            // different values conflict (rule 6).
                            GetReply::Values(stored) if stored != *values => {
                                Err(Refusal::RequestConflict.into())
                            }
                            // The item matched, or no longer exists or
                            // cannot be read: the kind 31 digest decides.
                            _ => {
                                submit.stage = Stage::Append;
                                Ok(())
                            }
                        }
                    }
                    Polled::Lost => {
                        submit.stage = Stage::Append;
                        Ok(())
                    }
                },
                stage @ (Stage::Lookup(_) | Stage::Append) => {
                    submit.stage = stage;
                    Ok(())
                }
            };
            match result {
                Ok(()) => kept.push(submit),
                Err(refused) => {
                    // Nothing was journaled; an item stored for it is not
                    // referenced (rule 2). A fresh put that failed stored
                    // nothing.
                    if submit.stored {
                        self.person_delete(submit.record.input);
                    }
                    self.out
                        .push_back(self.person_refuse(submit.request, refused));
                }
            }
        }
        // Keep the order: earlier submits append first.
        kept.append(&mut self.persons.submits);
        self.persons.submits = kept;
    }

    /// A fresh input's put reply (ordering rules 1 and 2).
    fn person_put(&mut self, submit: &mut Submit, reply: PutReply) -> Result<(), Refusing> {
        match reply {
            PutReply::Stored | PutReply::ExistsSame => {
                submit.stored = true;
                self.persons.items.insert(submit.record.input);
                submit.stage = Stage::Append;
                Ok(())
            }
            PutReply::ExistsDifferent => Err(Refusing {
                reason: Refusal::SecretStoreUnavailable,
                status: Some(DUPLICATE_ITEM),
            }),
            PutReply::Full => Err(Refusal::SecretStoreFull.into()),
            PutReply::Failed(failure) => {
                let (reason, status) = store_refusal(failure);
                Err(Refusing { reason, status })
            }
        }
    }

    /// Emits one `secret_store.*` audit event (operations and audit): the
    /// input, the reply code, the status of `unavailable` and counts only.
    fn audit_store(&self, event: Event) {
        if let Some(emitter) = &self.audit_emitter {
            let _ = emitter.try_emit(event);
        }
    }

    /// Sends the oldest ready submit's request lookup or kind 31 append
    /// when the ticket is free. Returns whether it took the ticket.
    fn drive_submits(&mut self) -> bool {
        if self.pending.is_some() || self.closing || self.unavailable.is_some() {
            return false;
        }
        // Test-support seam (SS-I3): appends are held for this run when
        // the scratch home held `person-command-hold-append` at start;
        // lookups still run, so the item is put.
        #[cfg(feature = "test-support")]
        let hold = self.persons.hold_append;
        #[cfg(not(feature = "test-support"))]
        let hold = false;
        let Some(at) = self.persons.submits.iter().position(|s| match s.stage {
            Stage::Lookup(_) => true,
            Stage::Append => !hold,
            _ => false,
        }) else {
            return false;
        };
        let submit = self.persons.submits.remove(at);
        let lookup = matches!(submit.stage, Stage::Lookup(_));
        let command = if lookup {
            writer::Command::RequestOutcome {
                request: submit.record.request,
            }
        } else {
            writer::Command::QueuePersonCommand {
                command: submit.record.clone(),
            }
        };
        let sent = self
            .writer
            .as_ref()
            .ok_or(writer::Error::Unavailable)
            .and_then(|w| w.try_submit(command));
        let job = if lookup {
            Job::PersonLookup(Box::new(submit))
        } else {
            Job::PersonQueue(Box::new(submit))
        };
        match sent {
            Ok(ticket) => {
                self.pending = Some((ticket, job));
                true
            }
            Err(cause) => {
                // Nothing was sent: a definite refusal.
                self.person_job_failed(job, cause);
                false
            }
        }
    }

    /// The kind 31 append's reply (or a refused submission).
    pub(super) fn person_queued(&mut self, submit: Submit, result: writer::Result<writer::Reply>) {
        match result {
            Ok(value) => {
                let accepted = value.accepted_input_id;
                if submit.stored && accepted != Some(submit.record.input) {
                    // An exact retry answered with the original input: this
                    // submit's own item is not referenced.
                    self.person_delete(submit.record.input);
                }
                if !submit.retry
                    && accepted == Some(submit.record.input)
                    && let Some(emitter) = &self.audit_emitter
                {
                    let _ = emitter.try_emit(Event::PersonCommandQueued {
                        project: submit.record.project,
                        conversation: submit.record.conversation,
                        input: submit.record.input,
                        request: submit.record.request,
                        outside_sandbox: submit.record.outside_sandbox,
                        secret_item: submit.stored,
                    });
                }
                self.queue_reply(submit.request, value, false);
            }
            Err(cause) => {
                // Ordering rules 3 and 5: an uncertain append keeps the
                // item for resolution by request ID; a definite rejection
                // deletes it.
                let uncertain = matches!(
                    cause,
                    writer::Error::OutcomeUnconfirmed | writer::Error::RepairRequired
                );
                if submit.stored && !uncertain {
                    self.person_delete(submit.record.input);
                }
                if cause == writer::Error::OutcomeUnconfirmed {
                    self.unavailable = Some(storage_error(cause));
                }
                let refusal = error_refusal(cause);
                self.out
                    .push_back(self.person_refuse(submit.request, refusal.into()));
            }
        }
    }

    /// A submit or dispatch-read error before the ticket took it.
    pub(super) fn person_job_failed(&mut self, job: Job, cause: writer::Error) {
        match job {
            Job::PersonQueue(submit) => self.person_queued(*submit, Err(cause)),
            Job::PersonLookup(submit) => {
                // Nothing was stored or journaled for it.
                if cause == writer::Error::OutcomeUnconfirmed {
                    self.unavailable = Some(storage_error(cause));
                }
                let refusal = error_refusal(cause);
                self.out
                    .push_back(self.person_refuse(submit.request, refusal.into()));
            }
            Job::PersonRead(input) => {
                // The read failed: the input stays queued and is read
                // again on the next pass.
                self.persons.dispatches.remove(&input);
            }
            Job::PersonCheck(_) => self.persons.checking = None,
            Job::PersonHold(input) => {
                self.persons.dispatches.remove(&input);
                if !matches!(
                    cause,
                    writer::Error::JournalQuota | writer::Error::JournalDiskSpace
                ) {
                    tracing::warn!(class = storage_error(cause), "person_command_hold_failed");
                }
            }
            _ => {}
        }
    }

    // ----- Queue replies -----

    /// A queue reply with kind 3 entries filled from their kind 31.
    pub(super) fn queue_reply(
        &mut self,
        request: pb::Envelope,
        value: writer::Reply,
        full_text: bool,
    ) {
        let request_id = match &request.body {
            Some(Body::ConversationQueueSubmit(v)) => v.request_id.clone(),
            Some(Body::ConversationQueueCommandSubmit(v)) => v.request_id.clone(),
            Some(Body::ConversationQueueReorder(v)) => v.request_id.clone(),
            Some(Body::ConversationQueueDecision(v)) => v.request_id.clone(),
            _ => None,
        };
        let preparing = self.preparing_step();
        let waiting = self.waiting_reasons();
        let mut entries = Vec::with_capacity(value.inputs.len());
        for v in value.inputs {
            // A dispatch hold whose decision waits for the ticket keeps the
            // reason it noted.
            let hold_pending = matches!(
                self.persons.dispatches.get(&v.input.input),
                Some(Dispatch::Holding)
            ) || self.persons.holds.contains(&v.input.input);
            if v.status != InputStatus::Held && !hold_pending {
                self.hold_reasons.remove(&v.input.input);
            }
            let hold = (v.status == InputStatus::Held)
                .then(|| self.hold_reasons.get(&v.input.input).copied())
                .flatten();
            if let Some(person) = v.person.as_deref() {
                // A released `!!` job's entry is Complete in the overlay
                // (OD2); the job stays live.
                let state = if v.status == InputStatus::Running
                    && self.persons.released.contains(&v.input.input)
                {
                    InputStatus::Complete
                } else {
                    v.status
                };
                entries.push(queue_entry(
                    &v,
                    person,
                    full_text,
                    state,
                    hold,
                    waiting.get(&v.input.input),
                ));
                continue;
            }
            entries.push(pb::ConversationQueueEntry {
                command_cwd: None,
                command_outside_sandbox: None,
                waiting_reason: None,
                waiting_text: None,
                command_filtered: None,
                input_id: Some(v.input.input.to_vec()),
                project_id: Some(v.input.project.to_vec()),
                conversation_id: Some(v.input.conversation.to_vec()),
                target_operation_id: v.target_operation.map(|id| id.to_vec()),
                target_generation: Some(v.target_generation),
                kind: Some(v.kind as u32),
                state: Some(v.status as u32),
                text: Some(v.input.prompt),
                sequence: Some(v.sequence),
                operation_id: v.operation.map(|(id, _)| id.to_vec()),
                generation: v.operation.map(|(_, generation)| generation),
                new_conversation: Some(v.input.new_conversation),
                order_position: Some(v.order_position),
                hold_reason: hold.map(str::to_owned),
                waiting_step: preparing
                    .filter(|(input, _)| *input == v.input.input && v.status == InputStatus::Queued)
                    .map(|(_, step)| step as i32),
            });
        }
        self.out.push_back(reply(
            request,
            Body::ConversationQueueReply(pb::ConversationQueueReply {
                refusal: None,
                refusal_status: None,
                entries,
                request_id,
                full_text: Some(full_text),
                revision: self.replay.as_ref().map(|state| state.revision),
                pending: Some(false),
                order_revision: value.order_revision,
                stale_order: Some(value.stale_order),
                accepted_input_id: value.accepted_input_id.map(|id| id.to_vec()),
            }),
        ));
    }

    /// The waiting reason of each kind 3 lane head that waits (control API
    /// additions, queue integration): a Keychain wait, the controller's
    /// block reason for its waiter, a model turn of its conversation, or an
    /// earlier held input of its lane. Live state; never journaled.
    pub(super) fn waiting_reasons(&self) -> BTreeMap<Id, (i32, String)> {
        let mut out = BTreeMap::new();
        let Some(state) = self.replay.as_deref() else {
            return out;
        };
        let busy = state
            .active_operation
            .and_then(|op| state.operations.get(&op))
            .map(|op| op.conversation);
        let controller = self.jobs.controller_ref();
        for (input, entry) in &state.inputs {
            if entry.kind != InputKind::Person
                || state.input_status(*input) != Some(InputStatus::Queued)
            {
                continue;
            }
            // The first Queued or Held input of the lane, by order.
            let head = state
                .inputs
                .values()
                .filter(|q| {
                    q.conversation == entry.conversation
                        && matches!(
                            state.input_status(q.request),
                            Some(InputStatus::Queued | InputStatus::Held)
                        )
                })
                .min_by_key(|q| (q.kind != InputKind::Steer, q.order_position))
                .map(|q| q.request);
            let earlier_held = state.inputs.values().any(|q| {
                q.conversation == entry.conversation
                    && q.order_position < entry.order_position
                    && state.input_status(q.request) == Some(InputStatus::Held)
            });
            let reason = match self.persons.dispatches.get(input) {
                Some(Dispatch::Waiting(_, wait, _)) => {
                    let (code, text) = keychain_text(*wait);
                    Some((code, text.to_owned()))
                }
                Some(Dispatch::Submitted(job)) => controller
                    .and_then(|c| c.view(*job, Instant::now()))
                    .and_then(|view| match view.snapshot.state {
                        JobRunState::Queued(reason) => {
                            let text = view.text.filter(|t| {
                                !t.is_empty()
                                    && t.len() <= asura_control::person::MAX_WAITING_TEXT_BYTES
                                    && !t.chars().any(char::is_control)
                            })?;
                            Some((block_reason(reason), text))
                        }
                        _ => None,
                    }),
                Some(_) => None,
                None if busy == Some(entry.conversation) && head == Some(*input) => Some((
                    pb::QueueWaitReason::ModelTurn as i32,
                    "waiting: model turn running".to_owned(),
                )),
                None if earlier_held => Some((
                    pb::QueueWaitReason::HeldHead as i32,
                    "waiting: an earlier input is held".to_owned(),
                )),
                None => None,
            };
            if let Some(reason) = reason {
                out.insert(*input, reason);
            }
        }
        out
    }

    // ----- Dispatch -----

    /// Dispatches each ready person command (queue integration): the lane
    /// head with no model turn in its conversation, one per lane. Reads its
    /// kind 31, then its item, then submits the person start.
    fn drive_person_dispatch(&mut self, now: Instant) {
        if self.closing || self.unavailable.is_some() {
            return;
        }
        let Some(state) = self.replay.clone() else {
            return;
        };
        if self.jobs.controller_ref().is_none() {
            return;
        }
        for input in state.ready_person_inputs(&self.persons.released) {
            self.persons
                .dispatches
                .entry(input)
                .or_insert(Dispatch::Reading);
        }
        // Advance the item reads, the paced rechecks and the starts.
        let inputs: Vec<Id> = self.persons.dispatches.keys().copied().collect();
        for input in inputs {
            self.advance_dispatch(input, now);
        }
    }

    fn advance_dispatch(&mut self, input: Id, now: Instant) {
        let Some(dispatch) = self.persons.dispatches.remove(&input) else {
            return;
        };
        let next = match dispatch {
            Dispatch::Ready(record) => self.person_read_item(record, None, now),
            Dispatch::Getting(record, ticket, pacing) => match ticket.poll() {
                Polled::Pending => Dispatch::Getting(record, ticket, pacing),
                Polled::Ready(reply) => {
                    let (code, status) = get_reply(&reply);
                    self.audit_store(Event::SecretStoreGet {
                        input: record.input,
                        reply: code,
                        status,
                    });
                    self.person_item_read(record, reply, pacing, now)
                }
                Polled::Lost => {
                    Dispatch::Waiting(record, KeychainWait::Unavailable, paced(pacing, now))
                }
            },
            Dispatch::Admit(record, values) => self.person_start(record, values.map(|v| *v), now),
            Dispatch::Waiting(record, wait, pacing) => {
                let due = now >= pacing.at()
                    || (wait == KeychainWait::Busy
                        && self.persons.store.as_ref().is_some_and(Store::has_room));
                if due {
                    self.person_read_item(record, Some(pacing), now)
                } else {
                    Dispatch::Waiting(record, wait, pacing)
                }
            }
            other => other,
        };
        self.persons.dispatches.insert(input, next);
    }

    /// The dispatch read of the item, or the start for an input without
    /// one.
    fn person_read_item(
        &mut self,
        record: Box<PersonCommandQueued>,
        pacing: Option<Pacing>,
        now: Instant,
    ) -> Dispatch {
        let needs_item =
            record.outside_sandbox || !record.number_kinds.is_empty() || record.whole_command;
        if !needs_item {
            return self.person_start(record, None, now);
        }
        self.persons.items.insert(record.input);
        let Some(state) = self.replay.as_deref() else {
            return Dispatch::Ready(record);
        };
        let Some(store) = &self.persons.store else {
            return Dispatch::Waiting(record, KeychainWait::Unavailable, paced(pacing, now));
        };
        let binding = secret_store::binding(
            &state.installation_id,
            &record.project,
            &record.conversation,
            &record.input,
            &record.digest,
        );
        match store.get(record.input, binding) {
            Ok(ticket) => Dispatch::Getting(record, ticket, pacing),
            Err(Refused::Busy) => Dispatch::Waiting(record, KeychainWait::Busy, paced(pacing, now)),
            Err(Refused::Stopped) => {
                Dispatch::Waiting(record, KeychainWait::Unavailable, paced(pacing, now))
            }
        }
    }

    /// The item read at dispatch (secret store failures).
    fn person_item_read(
        &mut self,
        record: Box<PersonCommandQueued>,
        reply: GetReply,
        pacing: Option<Pacing>,
        now: Instant,
    ) -> Dispatch {
        match reply {
            GetReply::Values(values) => self.person_start(record, Some(values), now),
            GetReply::Missing => self.command_hold(record.input, SECRETS_LOST),
            GetReply::Mismatch => {
                self.person_delete(record.input);
                self.command_hold(record.input, SECRETS_LOST)
            }
            GetReply::Failed(StoreFailure::Denied) => {
                self.command_hold(record.input, SECRET_STORE_DENIED)
            }
            GetReply::Failed(StoreFailure::Locked) => {
                Dispatch::Waiting(record, KeychainWait::Locked, paced(pacing, now))
            }
            GetReply::Failed(StoreFailure::Unavailable { .. }) => {
                Dispatch::Waiting(record, KeychainWait::Unavailable, paced(pacing, now))
            }
        }
    }

    /// Builds the person grant's start and submits it to the controller.
    fn person_start(
        &mut self,
        record: Box<PersonCommandQueued>,
        values: Option<Secrets>,
        now: Instant,
    ) -> Dispatch {
        let Some(Ok(rules)) = self.rules.need(self.wake.as_ref()) else {
            // The rule set warms first (SD-S2); the start waits.
            return Dispatch::Admit(record, values.map(Box::new));
        };
        // Rule 4: a changed rule set holds the input; the item stays.
        if values.is_some()
            && (record.rule_set_revision != rules.revision()
                || record.rule_set_digest != rules.digest())
        {
            return self.command_hold(record.input, SECRET_RULES_CHANGED);
        }
        let Some((path, device, inode, boundary)) = self.project_identity(record.project) else {
            // Not resident: the next pass after a project page reads again.
            return Dispatch::Admit(record, values.map(Box::new));
        };
        if self.jobs.controller_ref().is_none() {
            return Dispatch::Admit(record, values.map(Box::new));
        }
        let values = values.unwrap_or_default();
        // The numbered values, or the whole-command value, re-planned.
        let (variables, command, literals) = if record.whole_command {
            let Some(raw) = values
                .whole_command
                .as_ref()
                .and_then(|r| std::str::from_utf8(r).ok())
            else {
                self.person_delete(record.input);
                return self.command_hold(record.input, SECRETS_LOST);
            };
            match secrets::rewrite_exec_cmd_call_with(rules, raw, &record.cwd) {
                Ok(Plan::WholeCommand(whole)) => {
                    let (display, _) = cut_display(&whole.display);
                    let kinds: Vec<_> = whole
                        .variables
                        .iter()
                        .take(record.number_kinds.len())
                        .map(SecretVariable::kind)
                        .collect();
                    if display != record.display_command || kinds != record.number_kinds {
                        self.person_delete(record.input);
                        return self.command_hold(record.input, SECRETS_LOST);
                    }
                    (Vec::new(), Some(whole.command), whole.variables)
                }
                _ => {
                    self.person_delete(record.input);
                    return self.command_hold(record.input, SECRETS_LOST);
                }
            }
        } else if record.number_kinds.is_empty() {
            (Vec::new(), None, Vec::new())
        } else {
            match replan_rewrite(rules, &record, &values.variables) {
                Some(variables) => {
                    let literals = Vec::new();
                    (variables, None, literals)
                }
                None => {
                    self.person_delete(record.input);
                    return self.command_hold(record.input, SECRETS_LOST);
                }
            }
        };
        let launch = if record.outside_sandbox {
            let Some(environment) = values.launch_environment.as_ref() else {
                self.person_delete(record.input);
                return self.command_hold(record.input, SECRETS_LOST);
            };
            PersonLaunch::Outside {
                launch: environment
                    .iter()
                    .map(|e| (e.name.to_vec(), e.value.to_vec()))
                    .collect(),
            }
        } else {
            PersonLaunch::Sandboxed {
                path: record.search_path.as_bytes().to_vec(),
                client: record
                    .environment
                    .iter()
                    .map(|(variable, value)| {
                        let name = asura_control::person::CLIENT_VARIABLES
                            [usize::from(*variable as u8) - 1];
                        (name.as_bytes().to_vec(), value.as_bytes().to_vec())
                    })
                    .collect(),
            }
        };
        let redaction = if literals.is_empty() {
            JobRedaction::new(rules, &variables)
        } else {
            JobRedaction::new(rules, &literals)
        };
        let request = StartRequest {
            origin: Origin::Person {
                input: record.input,
            },
            agent: AgentKey {
                conversation: record.conversation,
                agent: 0,
            },
            scope: tools::ExecutionScope {
                project: record.project,
                path,
                device,
                inode,
                boundary,
            },
            // The profile ID comes from kind 31 field 7 bit 1 (person grant).
            profile: if record.outside_sandbox {
                Profile::PersonOutside1
            } else {
                Profile::Person1
            },
            command: record.command.clone(),
            cwd: record.cwd.clone(),
            secrets: variables,
            secret_command: command,
            redaction,
            duplicate_digest: record.digest,
            stored_command: record.command_text().to_owned(),
            person: Some(launch),
        };
        let Some(controller) = self.jobs.controller() else {
            return Dispatch::Ready(record);
        };
        match controller.submit(request, now) {
            Admission::Accepted { job } | Admission::Existing { job } => {
                // The controller grants slots in its own poll, which ran
                // earlier in this reactor step: wake the reactor so the
                // start is evaluated now, not at the next unrelated wake.
                if let Some(wake) = &self.wake {
                    let _ = wake.notify();
                }
                Dispatch::Submitted(job)
            }
            // A known-size queue bound: the command stays at the lane head
            // and dispatch tries again on the next wake (AQ1). The values
            // moved into the refused request and were dropped; the next
            // try reads the item again.
            Admission::Busy(_) => Dispatch::Ready(record),
            Admission::Conflict => self.command_hold(record.input, CANCELLED),
        }
    }

    /// Queues a hold decision for a dispatch the owner cannot run.
    fn command_hold(&mut self, input: Id, reason: &'static str) -> Dispatch {
        self.note_hold(input, reason);
        if !self.persons.holds.contains(&input) {
            self.persons.holds.push_back(input);
        }
        Dispatch::Holding
    }

    /// Takes the ticket for the next kind 31 read, hold decision or check
    /// read. Returns whether it took the ticket.
    fn drive_person_journal(&mut self) -> bool {
        if self.pending.is_some() || self.unavailable.is_some() {
            return false;
        }
        if let Some(input) = self.persons.holds.pop_front() {
            if self
                .submit(
                    writer::Command::InputDecision {
                        request: asura_platform::random_id(),
                        input,
                        action: journal::InputAction::Hold,
                    },
                    Job::PersonHold(input),
                )
                .is_err()
            {
                self.persons.dispatches.remove(&input);
            }
            return true;
        }
        if self.closing {
            return false;
        }
        let read = self
            .persons
            .dispatches
            .iter()
            .find(|(_, d)| matches!(d, Dispatch::Reading))
            .map(|(input, _)| *input);
        if let Some(input) = read {
            let Some(project) = self
                .replay
                .as_ref()
                .and_then(|r| r.inputs.get(&input))
                .map(|q| q.project)
            else {
                self.persons.dispatches.remove(&input);
                return false;
            };
            // Mark it in flight so it is read once.
            self.persons.dispatches.insert(input, Dispatch::Holding);
            if self
                .submit(
                    writer::Command::Inputs {
                        project,
                        input: Some(input),
                    },
                    Job::PersonRead(input),
                )
                .is_err()
            {
                self.persons.dispatches.remove(&input);
            }
            return true;
        }
        if self.persons.checking.is_none()
            && let Some(input) = self.persons.checks.pop_front()
        {
            let Some(project) = self
                .replay
                .as_ref()
                .and_then(|r| r.inputs.get(&input))
                .map(|q| q.project)
            else {
                return false;
            };
            self.persons.checking = Some(input);
            if self
                .submit(
                    writer::Command::Inputs {
                        project,
                        input: Some(input),
                    },
                    Job::PersonCheck(input),
                )
                .is_err()
            {
                self.persons.checking = None;
            }
            return true;
        }
        false
    }

    /// The kind 31 read of a dispatch.
    pub(super) fn person_read(&mut self, input: Id, mut value: writer::Reply, now: Instant) {
        let person = value
            .inputs
            .iter_mut()
            .find(|v| v.input.input == input)
            .and_then(|v| v.person.take());
        let ready = self.replay.as_ref().is_some_and(|r| {
            r.ready_person_inputs(&self.persons.released)
                .contains(&input)
        });
        match person {
            Some(record) if ready => {
                self.persons
                    .dispatches
                    .insert(input, Dispatch::Ready(record));
                self.advance_dispatch(input, now);
            }
            _ => {
                self.persons.dispatches.remove(&input);
            }
        }
    }

    /// The kind 31 read of a dropped input: delete its item when it has
    /// one.
    pub(super) fn person_checked(&mut self, input: Id, value: writer::Reply) {
        self.persons.checking = None;
        let has_item = value
            .inputs
            .iter()
            .find(|v| v.input.input == input)
            .and_then(|v| v.person.as_deref())
            .is_some_and(|p| p.outside_sandbox || !p.number_kinds.is_empty() || p.whole_command);
        if has_item {
            self.person_delete(input);
        }
    }

    /// A hold decision committed (or was refused): the dispatch ends.
    pub(super) fn person_held(&mut self, input: Id) {
        self.persons.dispatches.remove(&input);
    }

    // ----- Items -----

    /// Sends `delete` for one input's item after its deciding record
    /// (ordering rule 5). A full worker queue leaves it for the next sweep.
    fn person_delete(&mut self, input: Id) {
        #[cfg(test)]
        self.persons.deleted.push(input);
        let Some(store) = &self.persons.store else {
            return;
        };
        match store.delete(input) {
            Ok(ticket) => self.persons.deletes.push((input, ticket)),
            Err(_) => tracing::info!("secret_item_delete_deferred_to_sweep"),
        }
    }

    /// A queue mutation committed: a durable Drop of a person input deletes
    /// its item; any action rechecks the conversation's Keychain waits.
    pub(super) fn person_mutated(&mut self, request: &pb::Envelope) {
        let Some(Body::ConversationQueueDecision(decision)) = request.body.as_ref() else {
            if let Some(Body::ConversationQueueReorder(reorder)) = request.body.as_ref() {
                let input = id(&reorder.input_id);
                self.recheck_input(input);
            }
            return;
        };
        let input = id(&decision.input_id);
        self.recheck_input(input);
        let Some(state) = self.replay.clone() else {
            return;
        };
        let Some(entry) = state.inputs.get(&input) else {
            return;
        };
        if entry.kind != InputKind::Person {
            return;
        }
        if state.input_status(input) == Some(InputStatus::Dropped) {
            self.persons.dispatches.remove(&input);
            if self.persons.items.contains(&input)
                || entry.person.as_ref().is_some_and(|p| p.outside_sandbox)
            {
                self.person_delete(input);
            } else if self.persons.checks.len() < CHECKS && !self.persons.checks.contains(&input) {
                self.persons.checks.push_back(input);
            }
        }
    }

    /// Before a Hold or Drop of a person input reaches the writer, its
    /// waiter leaves the controller (it has no record), so it never takes
    /// a slot afterwards.
    pub(super) fn person_withdraw(&mut self, input: Id) {
        if let Some(Dispatch::Submitted(job)) = self.persons.dispatches.get(&input)
            && let Some(controller) = self.jobs.controller()
        {
            let job = *job;
            // A waiter withdraws; a granted start before its kind 26 is
            // withdrawn too; a job with kind 26 is not affected by a queue
            // decision (the writer refuses it, PR5).
            if controller
                .view(job, Instant::now())
                .is_some_and(|v| v.sequence.is_none())
            {
                let _ = controller.cancel(
                    job,
                    Requester::Person,
                    journal::JobReason::PersonStop,
                    false,
                );
            }
        }
        if matches!(
            self.persons.dispatches.get(&input),
            Some(Dispatch::Submitted(_) | Dispatch::Waiting(..) | Dispatch::Ready(_))
        ) {
            self.persons.dispatches.remove(&input);
        }
    }

    fn recheck_input(&mut self, input: Id) {
        let Some(conversation) = self
            .replay
            .as_ref()
            .and_then(|r| r.inputs.get(&input))
            .map(|q| q.conversation)
        else {
            return;
        };
        self.recheck_conversation(&conversation);
    }
    fn recheck_conversation(&mut self, conversation: &[u8]) {
        let Some(state) = self.replay.as_deref() else {
            return;
        };
        let inputs: BTreeSet<Id> = state
            .inputs
            .values()
            .filter(|q| q.conversation.as_slice() == conversation)
            .map(|q| q.request)
            .collect();
        self.persons.recheck_now(&inputs);
    }

    // ----- Events, store and sweep -----

    /// The controller's events for person jobs: a settlement deletes the
    /// item (after kind 28 committed) and ends the overlay; `ShellExited`
    /// releases the lane (OD2); a withdrawn waiter whose input is still
    /// queued is held, so it never dispatches again without Resume.
    pub(super) fn person_events(&mut self, events: &[JobEvent]) {
        for event in events {
            match *event {
                JobEvent::ShellExited(job, _, _) => {
                    if let Some(input) = self.person_input(job) {
                        self.persons.released.insert(input);
                        self.step_refresh = true;
                    }
                }
                JobEvent::Settled(job, _) => {
                    if let Some(input) = self.person_input(job) {
                        self.persons.released.remove(&input);
                        self.persons.dispatches.remove(&input);
                        if self.persons.items.remove(&input) {
                            self.person_delete(input);
                        }
                    }
                }
                JobEvent::State(job, JobState::Withdrawn) => {
                    let found = self
                        .persons
                        .dispatches
                        .iter()
                        .find(|(_, d)| matches!(d, Dispatch::Submitted(j) if *j == job))
                        .map(|(input, _)| *input);
                    if let Some(input) = found {
                        self.persons.dispatches.remove(&input);
                        let queued = self
                            .replay
                            .as_ref()
                            .is_some_and(|r| r.input_status(input) == Some(InputStatus::Queued));
                        if queued && !self.closing {
                            self.command_hold(input, CANCELLED);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// The person input of a person-origin job.
    fn person_input(&self, job: Id) -> Option<Id> {
        let controller = self.jobs.controller_ref()?;
        match controller.view(job, Instant::now())?.origin {
            Origin::Person { input } => Some(input),
            Origin::Model { .. } => None,
        }
    }

    /// The store, the switch and the sweep: the start-time read gives the
    /// switch and the target, the replay the installation; the sweep runs
    /// once start-time recovery committed every `interrupted` settlement.
    fn poll_store(&mut self) {
        if let Some((outside, target)) = self.jobs.take_person() {
            // A persisted change that arrived first is newer.
            if self.persons.switch().is_none() {
                self.persons.set_switch(outside);
            }
            self.persons.target = Some(target);
            let switch = self.persons.switch.clone();
            if let Some(controller) = self.jobs.controller() {
                controller.set_outside_switch(Box::new(move || {
                    switch.load(Ordering::Acquire) != SWITCH_OFF
                }));
            }
        }
        if self.persons.store.is_none()
            && !self.persons.store_failed
            && let Some(Some(target)) = &self.persons.target
            && let Some(state) = self.replay.as_deref()
            && state.binding.is_some()
        {
            #[cfg(feature = "test-support")]
            let started = match crate::test_support::secret_store_gate() {
                Some(gate) => Store::gated(
                    target.clone(),
                    state.installation_id,
                    self.wake.clone(),
                    gate,
                ),
                None => Store::new(target.clone(), state.installation_id, self.wake.clone()),
            };
            #[cfg(not(feature = "test-support"))]
            let started = Store::new(target.clone(), state.installation_id, self.wake.clone());
            match started {
                Ok(store) => self.persons.store = Some(store),
                Err(_) => {
                    self.persons.store_failed = true;
                    tracing::warn!("secret_store_unavailable");
                }
            }
        }
        // Deletes: a reply moves the observation; nothing else waits on it.
        let mut deletes = std::mem::take(&mut self.persons.deletes);
        deletes.retain(|(input, ticket)| match ticket.poll() {
            Polled::Pending => true,
            Polled::Ready(reply) => {
                let (code, status) = delete_reply(&reply);
                self.audit_store(Event::SecretStoreDelete {
                    input: *input,
                    reply: code,
                    status,
                });
                false
            }
            Polled::Lost => false,
        });
        deletes.append(&mut self.persons.deletes);
        self.persons.deletes = deletes;
        // The sweep after replay and recovery (SS-F1).
        if self.persons.recovering.is_none()
            && self.jobs.replay_taken()
            && let Some(state) = self.replay.as_deref()
        {
            self.persons.recovering = Some(state.jobs.keys().copied().collect());
        }
        match std::mem::take(&mut self.persons.sweep) {
            Sweep::NotStarted => {
                let recovered = self.persons.recovering.as_ref().is_some_and(|start| {
                    self.replay
                        .as_ref()
                        .is_some_and(|r| start.iter().all(|job| !r.jobs.contains_key(job)))
                });
                if recovered
                    && let (Some(store), Some(state)) =
                        (&self.persons.store, self.replay.as_deref())
                {
                    let live: BTreeSet<[u8; 16]> = state
                        .inputs
                        .values()
                        .filter(|q| {
                            q.kind == InputKind::Person
                                && matches!(
                                    state.input_status(q.request),
                                    Some(
                                        InputStatus::Queued
                                            | InputStatus::Held
                                            | InputStatus::Running
                                    )
                                )
                        })
                        .map(|q| q.request)
                        .chain(self.persons.dispatches.keys().copied())
                        .collect();
                    match store.sweep(live) {
                        Ok(ticket) => self.persons.sweep = Sweep::Running(ticket),
                        Err(_) => self.persons.sweep = Sweep::NotStarted,
                    }
                } else {
                    self.persons.sweep = Sweep::NotStarted;
                }
            }
            Sweep::Running(ticket) => match ticket.poll() {
                Polled::Pending => self.persons.sweep = Sweep::Running(ticket),
                Polled::Ready(report) => {
                    tracing::info!(
                        kept = report.kept,
                        deleted = report.deleted,
                        failed = report.failed,
                        stranded = report.stranded,
                        unrecognized = report.unrecognized,
                        truncated = report.truncated,
                        "secret_store_swept"
                    );
                    self.audit_store(sweep_event(&report));
                    self.persons.sweep = Sweep::Done;
                }
                Polled::Lost => self.persons.sweep = Sweep::Done,
            },
            Sweep::Done => self.persons.sweep = Sweep::Done,
        }
    }

    /// The owner's person step: store and sweep, submits, dispatches,
    /// released inputs that settled, and the published waiting reasons.
    pub(super) fn poll_persons(&mut self, now: Instant) {
        self.poll_store();
        self.poll_submits();
        self.drive_person_dispatch(now);
        if let Some(state) = self.replay.as_deref() {
            self.persons
                .released
                .retain(|input| state.input_status(*input) == Some(InputStatus::Running));
        }
        let waiting = self.waiting_reasons();
        if waiting != self.persons.published {
            self.persons.published = waiting;
            self.step_refresh = true;
            for (_, deadline) in &mut self.observations {
                *deadline = now;
            }
        }
    }

    /// The person journal work, after the turn's and the controller's
    /// records and before new foreground commands: holds, kind 31 reads,
    /// check reads, then kind 31 appends.
    pub(super) fn drive_persons(&mut self) -> bool {
        self.drive_person_journal() || self.drive_submits()
    }
}

/// The paced Keychain recheck (secret store failures): the first wait
/// starts the shared pacing at 1 s; each later miss takes the next delay
/// (1, 1, 2, 3, 5, 8 s and so on, at most 60 s apart). It ends nothing.
fn paced(pacing: Option<Pacing>, now: Instant) -> Pacing {
    match pacing {
        Some(mut pacing) => {
            pacing.advance(now);
            pacing
        }
        None => Pacing::start(now),
    }
}

impl Owner {
    /// A person command under dispatch: its kind 31 or item read, a
    /// Keychain wait, or a waiter the controller holds before kind 26.
    pub(super) fn person_dispatching(&self, input: Id) -> bool {
        match self.persons.dispatches.get(&input) {
            Some(Dispatch::Submitted(job)) => self
                .jobs
                .controller_ref()
                .and_then(|c| c.view(*job, Instant::now()))
                .is_some_and(|v| v.sequence.is_none()),
            Some(Dispatch::Holding) | None => false,
            Some(_) => true,
        }
    }
    /// A persisted `personCommands.allowOutsideSandbox` change (PCR23): the
    /// next `!!` submit and slot grant read it.
    pub(crate) fn set_allow_outside(&mut self, on: bool) {
        self.persons.set_switch(on);
    }
    /// `ServiceObservation` field 8.
    pub(crate) fn secret_store_status(&self) -> Option<pb::SecretStoreStatus> {
        self.persons.observation()
    }
}

#[cfg(test)]
impl Owner {
    /// Owner unit tests: stops the secret store worker and waits for it,
    /// before the test deletes its scratch keychain (testing rule 2).
    pub(crate) fn close_secret_store(&mut self) {
        if let Some(store) = self.persons.store.take() {
            let _ = store.close().join();
        }
    }
    /// Owner unit tests: closes the journal writer and waits until its
    /// worker settled, as a crash leaves the journal (no closure record is
    /// written), so the next owner of the process can open its writer.
    #[cfg(feature = "test-support")]
    pub(crate) fn close_writer_for_test(&mut self) {
        self.fixture_writer_join();
    }
    /// Owner unit tests: the controller's slots come from `slots`.
    pub(crate) fn set_test_slots(&mut self, slots: crate::job_controller::SlotFactory) {
        self.jobs.set_slots(slots);
    }
}

#[cfg(all(test, feature = "test-support"))]
#[path = "person_command_tests.rs"]
mod tests;
