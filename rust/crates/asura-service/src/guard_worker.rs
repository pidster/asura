//! Per-request owner validation isolated from the reactor. Never reuse a ticket.
use asura_platform::{OwnerGuard, OwnerValidation};
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

enum Action {
    Validate(OwnerValidation),
    Cleanup(OwnerGuard),
}
struct Finished {
    result: Result<(), asura_platform::Error>,
    guard: Option<OwnerGuard>,
}
pub(crate) enum Event {
    Deadline {
        ticket: u64,
    },
    Finished {
        ticket: u64,
        result: Result<(), asura_platform::Error>,
        guard: Option<OwnerGuard>,
        expired: bool,
    },
}
pub(crate) struct Worker {
    task: Option<JoinHandle<Finished>>,
    ticket: u64,
    deadline: Instant,
    cancel: Arc<AtomicBool>,
    deadline_reported: bool,
    completion: crate::completion::Completion,
}
impl Worker {
    pub(crate) fn start(snapshot: OwnerValidation, ticket: u64, now: Instant) -> io::Result<Self> {
        Self::launch(Action::Validate(snapshot), ticket, now).map_err(|(error, _)| error)
    }
    pub(crate) fn start_cleanup(
        guard: OwnerGuard,
        ticket: u64,
        now: Instant,
    ) -> Result<Self, (io::Error, OwnerGuard)> {
        Self::launch(Action::Cleanup(guard), ticket, now).map_err(|(error, action)| {
            let Action::Cleanup(guard) = action else {
                unreachable!("cleanup action retained")
            };
            (error, guard)
        })
    }
    fn launch(action: Action, ticket: u64, now: Instant) -> Result<Self, (io::Error, Action)> {
        let deadline = now + Duration::from_secs(2);
        let cancel = Arc::new(AtomicBool::new(false));
        let token = cancel.clone();
        let completion = crate::completion::Completion::default();
        let signal = completion.clone();
        // Transfer authority only after spawn succeeds. Spawn failure preserves the guard.
        let (sender, receiver) = mpsc::sync_channel::<Action>(1);
        let task = match thread::Builder::new()
            .name("asura-owner-check".into())
            .spawn(move || {
                let _settlement = signal.guard();
                let action = match receiver.recv() {
                    Ok(action) => action,
                    Err(_) => {
                        return Finished {
                            result: Err(asura_platform::Error::Unavailable),
                            guard: None,
                        };
                    }
                };
                let permitted = || !token.load(Ordering::Acquire) && Instant::now() < deadline;
                match action {
                    Action::Validate(snapshot) => {
                        let result = if permitted() {
                            snapshot.validate()
                        } else {
                            Err(asura_platform::Error::Deadline)
                        };
                        Finished {
                            result: if permitted() {
                                result
                            } else {
                                Err(asura_platform::Error::Deadline)
                            },
                            guard: None,
                        }
                    }
                    Action::Cleanup(mut guard) => {
                        let result = if permitted() {
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                guard.remove_endpoint()
                            }))
                            .unwrap_or(Err(asura_platform::Error::Unavailable))
                        } else {
                            Err(asura_platform::Error::Deadline)
                        };
                        // Preserve actual cleanup outcome, even if it completed after its deadline.
                        Finished {
                            result,
                            guard: Some(guard),
                        }
                    }
                }
            }) {
            Ok(task) => task,
            Err(error) => return Err((error, action)),
        };
        if let Err(error) = sender.try_send(action) {
            return Err((
                io::Error::other("guard worker handoff failed"),
                match error {
                    mpsc::TrySendError::Full(action) | mpsc::TrySendError::Disconnected(action) => {
                        action
                    }
                },
            ));
        }
        Ok(Self {
            task: Some(task),
            ticket,
            deadline,
            cancel,
            deadline_reported: false,
            completion,
        })
    }
    pub(crate) fn register(&self, wake: asura_platform::events::WakeSender) {
        self.completion.register(wake);
    }
    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
    pub(crate) fn settled(&self) -> bool {
        self.task.is_none()
    }
    pub(crate) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if self.settled() {
            None
        } else if self.completion.completed() {
            self.completion.settlement_deadline(now)
        } else if !self.deadline_reported {
            Some(self.deadline)
        } else {
            None
        }
    }
    pub(crate) fn poll(&mut self, now: Instant) -> Option<Event> {
        if self.task.as_ref().is_some_and(JoinHandle::is_finished) {
            let expired = self.deadline_reported
                || self.cancel.load(Ordering::Acquire)
                || now >= self.deadline;
            let finished = self.task.take().unwrap().join().unwrap_or(Finished {
                result: Err(asura_platform::Error::Unavailable),
                guard: None,
            });
            return Some(Event::Finished {
                ticket: self.ticket,
                result: finished.result,
                guard: finished.guard,
                expired,
            });
        }
        if self.task.is_some() && now >= self.deadline && !self.deadline_reported {
            self.deadline_reported = true;
            self.cancel();
            return Some(Event::Deadline {
                ticket: self.ticket,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timed_out_validation_retains_slot_and_cannot_authorize_late_success() {
        let completion = crate::completion::Completion::default();
        let signal = completion.clone();
        let (release, wait) = mpsc::channel();
        let task = thread::spawn(move || {
            let _settlement = signal.guard();
            wait.recv().unwrap();
            Finished {
                result: Ok(()),
                guard: None,
            }
        });
        let now = Instant::now();
        let mut worker = Worker {
            task: Some(task),
            ticket: 17,
            deadline: now,
            cancel: Arc::new(AtomicBool::new(false)),
            deadline_reported: false,
            completion,
        };
        assert!(matches!(
            worker.poll(now),
            Some(Event::Deadline { ticket: 17 })
        ));
        assert!(!worker.settled());
        assert!(worker.poll(now).is_none());
        release.send(()).unwrap();
        let limit = Instant::now() + Duration::from_secs(1);
        while !worker.task.as_ref().unwrap().is_finished() {
            assert!(Instant::now() < limit);
            thread::yield_now();
        }
        assert!(matches!(
            worker.poll(Instant::now()),
            Some(Event::Finished {
                ticket: 17,
                expired: true,
                ..
            })
        ));
        assert!(worker.settled());
        assert!(worker.poll(Instant::now()).is_none());
    }
    #[test]
    fn cleanup_returns_owner_and_completion_wakes_after_registration() {
        use std::os::{fd::AsRawFd, unix::fs::DirBuilderExt};
        let path = std::path::PathBuf::from(format!(
            "/private/tmp/asura-guard-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let runtime = asura_platform::RuntimeDirectory::scratch(&path, true).unwrap();
        let mut guard = runtime.acquire_owner().unwrap();
        let listener = guard.bind().unwrap();
        let mut worker = match Worker::start_cleanup(guard, 3, Instant::now()) {
            Ok(worker) => worker,
            Err(_) => panic!("spawn cleanup"),
        };
        let (wake, sender) = asura_platform::events::wake_pair().unwrap();
        worker.register(sender);
        let limit = Instant::now() + Duration::from_secs(2);
        let returned = loop {
            if let Some(Event::Finished {
                ticket,
                result,
                guard,
                expired,
            }) = worker.poll(Instant::now())
            {
                assert_eq!(ticket, 3);
                assert!(!expired);
                result.unwrap();
                break guard.unwrap();
            }
            assert!(Instant::now() < limit);
            asura_platform::poll(
                &[asura_platform::PollInterest {
                    fd: wake.as_raw_fd(),
                    read: true,
                    write: false,
                }],
                Duration::from_millis(10),
            )
            .unwrap();
        };
        assert!(!path.join(".asura/run/control.sock").exists());
        assert!(matches!(
            runtime.acquire_owner(),
            Err(asura_platform::Error::OwnerBusy)
        ));
        drop(returned);
        drop(listener);
        drop(worker);
        drop(runtime);
        std::fs::remove_dir_all(path).unwrap();
    }
}
