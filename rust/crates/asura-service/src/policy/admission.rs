//! The admission worker (security-policy.md, PL3 implementation contract):
//! one platform thread, `asura-policy-admission`, owned by the policy owner.
//! It runs every blocking step of policy admission off the reactor: the
//! solver package manifest read, SymCC compilation and the solver process
//! (copy, spawn, pipes, kill and reap), and the policy text store reads and
//! writes. The owner holds the FIFO of candidates (at most 16) and sends
//! this thread one job at a time; every call here on the reactor is a
//! channel operation.
use asura_platform::events::WakeSender;
use asura_platform::solver::{
    ExchangeEnd, Flow, SolverCancel, SolverError, SolverLimits, SolverPackage, SolverProcess,
    Stream,
};
use asura_platform::{RuntimeDirectory, WalkMode, canonical_path};
use asura_policy as ap;
use asura_policy_admission as adm;
use asura_storage::policy_texts::{PolicyTexts, TextError};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Where the solver package comes from (PL3 solver source).
#[derive(Clone, Debug)]
pub(crate) enum SolverSource {
    /// Production builds in PL3, and owners built for unit tests.
    None,
    /// A package directory: the development package or a test fixture.
    Package(PathBuf),
}

/// The solver state of `/status` (PL3 control and presentation).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SolverState {
    None,
    Available,
    Unavailable,
}
impl SolverState {
    pub(crate) fn code(self) -> u32 {
        match self {
            Self::None => 1,
            Self::Available => 2,
            Self::Unavailable => 3,
        }
    }
}

/// Where a candidate came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Origin {
    Person,
    Rollback(u64),
    Reprove,
}

/// One user-scope text to read back at restart or for a rollback.
#[derive(Clone, Debug)]
pub(crate) struct StoredText {
    pub digest: [u8; 32],
    /// `builtin`: the running build's default user text.
    pub builtin: bool,
}

pub(super) enum Work {
    /// Read a stored text (restart, rollback) and strict-validate it.
    Load {
        token: u64,
        text: StoredText,
        scope: ap::Scope,
    },
    /// Admit one candidate, then pin it when ready.
    Admit {
        token: u64,
        scope: ap::Scope,
        text: String,
        installation: String,
        user_default: String,
        previous: Option<String>,
    },
}

/// The result of one admission.
pub(crate) struct Admitted {
    pub admission: adm::Admission,
    /// The pinned text digest; `Err` refuses the candidate
    /// `storage_unavailable`.
    pub pinned: Option<Result<[u8; 32], ()>>,
    /// The solver package digest, when the solver ran.
    pub solver_digest: Option<[u8; 32]>,
    /// The cause code of a solver failure, for `policy.solver_failed`.
    pub solver_cause: Option<&'static str>,
    pub peak_footprint: u64,
}

pub(super) enum Done {
    /// The package manifest was read (or there is none).
    Solver(SolverState, Option<[u8; 32]>),
    Loaded {
        token: u64,
        result: Result<String, String>,
    },
    Admitted {
        token: u64,
        result: Box<Admitted>,
    },
}

/// The cancel of the running job: the flag that SymCC's loop reads and
/// the platform cancel of the solver process, when one runs.
#[derive(Default)]
pub(super) struct Cancel {
    flag: AtomicBool,
    solver: Mutex<Option<SolverCancel>>,
}
impl Cancel {
    pub(super) fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        if let Some(solver) = self
            .solver
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            solver.cancel();
        }
    }
    fn reset(&self) {
        self.flag.store(false, Ordering::Release);
    }
}

pub(super) struct Worker {
    sender: Option<SyncSender<Work>>,
    handle: Option<JoinHandle<()>>,
    replies: Receiver<Done>,
    pub(super) cancel: Arc<Cancel>,
}
impl Worker {
    pub(super) fn start(
        runtime: Option<RuntimeDirectory>,
        source: SolverSource,
        limits: SolverLimits,
        wake: Option<WakeSender>,
    ) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Work>(1);
        let (reply, replies) = mpsc::channel();
        let cancel = Arc::new(Cancel::default());
        let shared = cancel.clone();
        let handle = std::thread::Builder::new()
            .name("asura-policy-admission".into())
            .spawn(move || worker_main(runtime, source, limits, receiver, reply, wake, shared))
            .ok();
        Self {
            sender: handle.as_ref().map(|_| sender),
            handle,
            replies,
            cancel,
        }
    }
    /// Send one job; `false` when the thread is busy or gone.
    pub(super) fn send(&self, work: Work) -> bool {
        self.cancel.reset();
        matches!(self.sender.as_ref().map(|s| s.try_send(work)), Some(Ok(())))
    }
    pub(super) fn try_recv(&self) -> Option<Done> {
        self.replies.try_recv().ok()
    }
    /// The thread ended unexpectedly: joined; the caller replaces it.
    pub(super) fn lost(&mut self) -> bool {
        if self.handle.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            return self.sender.is_some();
        }
        false
    }
    /// Shutdown: cancel the running job (the solver is killed and reaped)
    /// and close the queue.
    pub(super) fn close(&mut self) {
        self.cancel.cancel();
        self.sender = None;
    }
    pub(super) fn settled(&mut self) -> bool {
        if self.handle.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(handle) = self.handle.take()
        {
            let _ = handle.join();
        }
        self.handle.is_none()
    }
}

/// The admission crate's solver pipe over the platform solver process.
/// The process starts at the first exchange of an admission, so a
/// candidate that needs no proof never starts one.
struct Pipe<'a> {
    runtime: Option<&'a RuntimeDirectory>,
    package: Option<&'a SolverPackage>,
    limits: SolverLimits,
    cancel: &'a Cancel,
    process: Option<SolverProcess>,
    cause: Option<&'static str>,
    used: bool,
}
impl Pipe<'_> {
    fn end(&mut self) -> u64 {
        let exit = match self.process.take() {
            Some(process) if self.cause.is_some() => process.kill(),
            Some(process) => process.finish(),
            None => return 0,
        };
        if exit.memory_bound && self.cause.is_none() {
            self.cause = Some("memory_bound");
        }
        exit.peak_footprint
    }
}
impl adm::SolverPipe for Pipe<'_> {
    fn exchange(
        &mut self,
        input: &[u8],
        output: &mut dyn FnMut(adm::PipeStream, &[u8]) -> adm::PipeFlow,
    ) -> Result<(), adm::PipeEnd> {
        if self.cause.is_some() {
            return Err(adm::PipeEnd::Fault);
        }
        if self.process.is_none() {
            let (Some(runtime), Some(package)) = (self.runtime, self.package) else {
                self.cause = Some("unavailable");
                return Err(adm::PipeEnd::Closed);
            };
            let cancel = match SolverCancel::new() {
                Ok(c) => c,
                Err(_) => {
                    self.cause = Some("io");
                    return Err(adm::PipeEnd::Io);
                }
            };
            *self.cancel.solver.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel.clone());
            if self.cancel.flag.load(Ordering::Acquire) {
                return Err(adm::PipeEnd::Cancelled);
            }
            match SolverProcess::start(runtime, package, self.limits, &cancel) {
                Ok(process) => self.process = Some(process),
                Err(error) => {
                    self.cause = Some(match error {
                        SolverError::Cancelled => return Err(adm::PipeEnd::Cancelled),
                        other => other.cause(),
                    });
                    return Err(adm::PipeEnd::Closed);
                }
            }
            self.used = true;
        }
        let process = self.process.as_mut().expect("started");
        let mut forward = |stream: Stream, bytes: &[u8]| {
            let stream = match stream {
                Stream::Stdout => adm::PipeStream::Stdout,
                Stream::Stderr => adm::PipeStream::Stderr,
            };
            match output(stream, bytes) {
                adm::PipeFlow::More => Flow::More,
                adm::PipeFlow::Answered => Flow::Answered,
                adm::PipeFlow::Fault => Flow::Fault,
            }
        };
        match process.exchange(input, &mut forward) {
            Ok(()) => Ok(()),
            Err(end) => {
                let (cause, end) = match end {
                    ExchangeEnd::Closed => ("closed", adm::PipeEnd::Closed),
                    ExchangeEnd::Cancelled => ("cancelled", adm::PipeEnd::Cancelled),
                    ExchangeEnd::MemoryBound => ("memory_bound", adm::PipeEnd::MemoryBound),
                    ExchangeEnd::Fault => ("protocol", adm::PipeEnd::Fault),
                    ExchangeEnd::Io(_) => ("io", adm::PipeEnd::Io),
                };
                self.cause = Some(cause);
                Err(end)
            }
        }
    }
}

/// The literal check of admission step 4: the platform canonicaliser,
/// run here on the admission thread.
struct Literals;
impl adm::LiteralCheck for Literals {
    fn check(&self, literal: &str) -> adm::LiteralResult {
        match canonical_path(literal, WalkMode::Read, None) {
            Ok(found) if found.path == literal => adm::LiteralResult::Canonical,
            Ok(found) => adm::LiteralResult::Respell(found.path),
            Err(_) => adm::LiteralResult::Missing,
        }
    }
}

fn worker_main(
    runtime: Option<RuntimeDirectory>,
    source: SolverSource,
    limits: SolverLimits,
    receiver: Receiver<Work>,
    replies: mpsc::Sender<Done>,
    wake: Option<WakeSender>,
    cancel: Arc<Cancel>,
) {
    let notify = || {
        if let Some(wake) = &wake {
            let _ = wake.notify();
        }
    };
    let package = match &source {
        SolverSource::None => None,
        SolverSource::Package(directory) => SolverPackage::read(directory).ok(),
    };
    let state = match (&source, &package) {
        (SolverSource::None, _) => SolverState::None,
        (_, Some(_)) => SolverState::Available,
        (_, None) => SolverState::Unavailable,
    };
    if replies
        .send(Done::Solver(
            state,
            package.as_ref().map(SolverPackage::digest),
        ))
        .is_err()
    {
        return;
    }
    notify();
    let texts = || {
        runtime
            .as_ref()
            .and_then(|r| PolicyTexts::open(r, true).ok())
    };
    // The loop ends when the owner drops the sender (shutdown or
    // replacement); each job ends by its completion or the cancel.
    while let Ok(work) = receiver.recv() {
        let done = match work {
            Work::Load { token, text, scope } => {
                let result = if text.builtin {
                    Ok(ap::RevisionTexts::builtin()
                        .scopes
                        .into_iter()
                        .find(|s| s.scope == scope)
                        .map(|s| s.text)
                        .unwrap_or_default())
                } else {
                    match texts() {
                        None => Err("text_store_unavailable".to_owned()),
                        Some(store) => match store.get(&text.digest) {
                            Ok(t) => Ok(t),
                            Err(TextError::Missing) => Err("text_missing".to_owned()),
                            Err(TextError::Mismatch) => Err("text_mismatch".to_owned()),
                            Err(_) => Err("text_store_unavailable".to_owned()),
                        },
                    }
                };
                let result = result.and_then(|t| {
                    ap::validate_text(scope, &t)
                        .map(|()| t)
                        .map_err(|_| "text_invalid".to_owned())
                });
                Done::Loaded { token, result }
            }
            Work::Admit {
                token,
                scope,
                text,
                installation,
                user_default,
                previous,
            } => {
                let mut pipe = Pipe {
                    runtime: runtime.as_ref(),
                    package: package.as_ref(),
                    limits,
                    cancel: &cancel,
                    process: None,
                    cause: None,
                    used: false,
                };
                let admission = adm::admit(
                    &adm::Candidate { scope, text: &text },
                    &adm::Baseline {
                        installation: &installation,
                        user_default: &user_default,
                        previous: previous.as_deref(),
                    },
                    Some(&mut pipe),
                    &Literals,
                    &cancel.flag,
                );
                let peak = pipe.end();
                *cancel.solver.lock().unwrap_or_else(|e| e.into_inner()) = None;
                let solver_digest = pipe
                    .used
                    .then(|| package.as_ref().map(SolverPackage::digest))
                    .flatten();
                let solver_cause = match &admission.outcome {
                    adm::Outcome::Rejected {
                        reason: adm::Reason::SolverFailed,
                        cause,
                        ..
                    } => pipe.cause.or(cause.map(|c| c.as_str())),
                    _ => None,
                };
                let pinned = matches!(admission.outcome, adm::Outcome::Ready).then(|| {
                    texts()
                        .ok_or(())
                        .and_then(|store| store.put(&text).map_err(|_| ()))
                });
                Done::Admitted {
                    token,
                    result: Box::new(Admitted {
                        admission,
                        pinned,
                        solver_digest,
                        solver_cause,
                        peak_footprint: peak,
                    }),
                }
            }
        };
        if replies.send(done).is_err() {
            return;
        }
        notify();
    }
}

/// The service's solver source (PL3 solver source). Production builds have
/// none; a `test-support` build reads `policy-test-solver` in a scratch
/// home, or else `<helper directory>/solver`.
pub(crate) fn solver_source(runtime: &RuntimeDirectory) -> SolverSource {
    let _ = runtime;
    #[cfg(feature = "test-support")]
    {
        if let Some(home) = runtime.scratch_home_for_test() {
            let named = home.join("policy-test-solver");
            if let Ok(text) = std::fs::read_to_string(&named) {
                let path = text.trim();
                if path.len() <= 1024 && path.starts_with('/') {
                    return SolverSource::Package(PathBuf::from(path));
                }
                return SolverSource::None;
            }
        }
        if let Some(directory) = std::env::current_exe()
            .ok()
            .and_then(|e| e.canonicalize().ok())
            .and_then(|e| crate::model_owner::helper_directory(&e).map(|d| d.join("solver")))
        {
            return SolverSource::Package(directory);
        }
    }
    SolverSource::None
}
