//! Secret rule set readiness (secret-detection.md, rule set readiness,
//! SD-S2): the service's one warmer. At service start the conversation
//! owner spawns one retained thread, `asura-secret-rules`, that compiles the
//! rule set with `secrets::detection()`, wakes the reactor and exits.
//! Reactor code reads only `secrets::compiled()`, which never compiles and
//! never blocks.
//!
//! A failed spawn logs `secret_rules_warm_unavailable` and keeps the state
//! `not started`; the next turn start or person note that needs the set
//! spawns again ([`Warmer::need`]), as does one after a warm thread that
//! finished without a result (a compile panic, logged
//! `secret_rules_warm_failed`). A waiting user never retries by itself,
//! so a failing spawn causes no loop. No time bound applies anywhere.
use asura_platform::events::WakeSender;
use asura_platform::secrets::{self, RuleSet, SelfCheckError};
use std::thread::JoinHandle;

/// The compiled rule set, or `None` while it is not ready.
pub(crate) type Ready = Option<Result<&'static RuleSet, SelfCheckError>>;

/// How the warm thread is started; tests replace it to fail a spawn.
type Spawn = fn(Option<WakeSender>) -> std::io::Result<JoinHandle<()>>;

fn spawn(wake: Option<WakeSender>) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("asura-secret-rules".into())
        .spawn(move || {
            let _ = secrets::detection();
            if let Some(wake) = wake {
                let _ = wake.notify();
            }
        })
}

/// The warm state: not started, or warming (the thread runs or ran).
pub(crate) struct Warmer {
    thread: Option<JoinHandle<()>>,
    spawn: Spawn,
    /// The result read; tests replace it to hold the set not ready.
    compiled: fn() -> Ready,
    spawns: u32,
}
impl Default for Warmer {
    fn default() -> Self {
        Self {
            thread: None,
            spawn,
            compiled: secrets::compiled,
            spawns: 0,
        }
    }
}
impl Warmer {
    /// The rule set as reactor code may read it.
    pub(crate) fn ready(&self) -> Ready {
        (self.compiled)()
    }
    /// Service start: spawn the warm thread unless the set is ready.
    pub(crate) fn start(&mut self, wake: Option<&WakeSender>) {
        self.need(wake);
    }
    /// A turn start or person note needs the set: spawn again when no
    /// thread runs, because none was started or the last one has finished
    /// without a result (a compile panic). Returns the set when it is ready.
    pub(crate) fn need(&mut self, wake: Option<&WakeSender>) -> Ready {
        let ready = self.ready();
        if ready.is_none() && self.thread.as_ref().is_some_and(|t| t.is_finished()) {
            // Finished, so the join returns at once; a panic is its result.
            if let Some(thread) = self.thread.take()
                && thread.join().is_err()
            {
                tracing::warn!("secret_rules_warm_failed");
            }
        }
        if ready.is_none() && self.thread.is_none() {
            self.spawns += 1;
            match (self.spawn)(wake.cloned()) {
                Ok(thread) => self.thread = Some(thread),
                Err(_) => tracing::warn!("secret_rules_warm_unavailable"),
            }
        }
        ready
    }
    #[cfg(test)]
    pub(crate) fn failing() -> Self {
        Self {
            thread: None,
            spawn: |_| Err(std::io::ErrorKind::Other.into()),
            compiled: secrets::compiled,
            spawns: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SD-S2 (unit): a failed spawn keeps the state `not started` and each
    /// later need spawns again; once a thread runs no second one starts.
    #[test]
    fn sd_s2_failed_spawn_retries_on_need_and_one_thread_runs() {
        let mut failing = Warmer::failing();
        if secrets::compiled().is_none() {
            failing.start(None);
            assert_eq!(failing.spawns, 1);
            failing.need(None);
            assert_eq!(failing.spawns, 2, "a need retries the failed spawn");
            assert!(failing.thread.is_none());
        }
        let mut warmer = Warmer::default();
        warmer.start(None);
        let spawned = warmer.spawns;
        warmer.need(None);
        assert_eq!(warmer.spawns, spawned, "one warm thread at most");
        if let Some(thread) = warmer.thread.take() {
            thread.join().unwrap();
        }
        assert!(warmer.ready().is_some_and(|r| r.is_ok()));
        // Ready: nothing is spawned.
        let mut ready = Warmer::failing();
        assert!(ready.need(None).is_some());
        assert_eq!(ready.spawns, 0);
    }

    static RELEASE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// SD-S2 (unit): a warm thread that ends without a result (an injected
    /// compile panic) returns the state to `not started`, so the next need
    /// spawns again; a thread that still runs is never doubled.
    #[test]
    fn sd_s2_need_respawns_after_a_thread_ends_without_a_result() {
        let mut warmer = Warmer {
            thread: None,
            spawn: |_| {
                std::thread::Builder::new()
                    .name("asura-secret-rules-test".into())
                    .spawn(|| panic!("injected compile panic"))
            },
            compiled: || None,
            spawns: 0,
        };
        assert!(warmer.need(None).is_none());
        assert_eq!(warmer.spawns, 1);
        while !warmer.thread.as_ref().unwrap().is_finished() {
            std::thread::yield_now();
        }
        assert!(warmer.need(None).is_none());
        assert_eq!(
            warmer.spawns, 2,
            "a finished thread with no result respawns"
        );

        // A running thread is not doubled.
        let mut running = Warmer {
            thread: None,
            spawn: |_| {
                std::thread::Builder::new().spawn(|| {
                    while !RELEASE.load(std::sync::atomic::Ordering::Acquire) {
                        std::thread::park();
                    }
                })
            },
            compiled: || None,
            spawns: 0,
        };
        running.need(None);
        running.need(None);
        assert_eq!(running.spawns, 1, "one warm thread at most");
        let thread = running.thread.take().unwrap();
        RELEASE.store(true, std::sync::atomic::Ordering::Release);
        thread.thread().unpark();
        thread.join().unwrap();
        if let Some(thread) = warmer.thread.take() {
            let _ = thread.join();
        }
    }
}
