//! One retained worker's result notification, distinct from thread settlement.
use asura_platform::events::WakeSender;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
#[derive(Default)]
struct Shared {
    wake: OnceLock<WakeSender>,
    completed: AtomicBool,
}
#[derive(Clone, Default)]
pub(crate) struct Completion(Arc<Shared>);
impl Completion {
    pub fn register(&self, wake: WakeSender) {
        let _ = self.0.wake.set(wake);
        self.notify();
    }
    pub fn notify(&self) {
        if let Some(wake) = self.0.wake.get() {
            let _ = wake.notify();
        }
    }
    pub fn finish(&self) {
        self.0.completed.store(true, Ordering::Release);
        self.notify();
    }
    pub fn completed(&self) -> bool {
        self.0.completed.load(Ordering::Acquire)
    }
    pub fn settlement_deadline(&self, now: Instant) -> Option<Instant> {
        self.completed().then_some(now + Duration::from_millis(1))
    }
    pub fn guard(&self) -> Guard {
        Guard(self.clone())
    }
}
pub(crate) struct Guard(Completion);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    #[test]
    fn completion_before_or_after_registration_wakes_receiver() {
        for first in [true, false] {
            let completion = Completion::default();
            let (reader, sender) = asura_platform::events::wake_pair().unwrap();
            if first {
                completion.finish();
            }
            completion.register(sender);
            reader.drain().unwrap();
            if !first {
                completion.finish();
            } else {
                completion.register(completion.0.wake.get().unwrap().clone());
            }
            let ready = asura_platform::poll(
                &[asura_platform::PollInterest {
                    fd: reader.as_raw_fd(),
                    read: true,
                    write: false,
                }],
                Duration::from_secs(1),
            )
            .unwrap();
            assert!(!ready.is_empty());
            assert!(completion.completed());
            assert!(completion.settlement_deadline(Instant::now()).is_some());
        }
    }
}
