//! One project page read slot (CF06b project paging): the picker's edge
//! pages and the exact `only` read behind `/project select ID`. Registry
//! order and policy stay in the service; page validation is shared with the
//! list commands. The render and input loop never waits on this slot.
use super::observation::Resolver;
use crate::conversation::{Page, ProjectPage};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;

/// Why the page is read; the app applies the result accordingly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Purpose {
    /// A picker edge page that joins the window with this revision.
    Edge { window_revision: u64 },
    /// `/project select ID` for an ID outside the window; the draft that
    /// asked for it is kept until a selection succeeds.
    Select { draft: String },
}

#[derive(Clone, Debug)]
pub(super) struct Request {
    pub read: ProjectPage,
    pub purpose: Purpose,
}

pub(super) struct Update {
    pub request: Request,
    pub result: Result<Page, String>,
}

/// One page read. It has no time bound (owner, 2026-10-16): the reply, the
/// person's cancel, exit or a lost connection ends it.
struct Job {
    handle: JoinHandle<Update>,
    request: Request,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
pub(super) struct Worker {
    notice: super::events::Notice,
    job: Option<Job>,
}

impl Worker {
    pub fn set_notice(&mut self, notice: super::events::Notice) {
        self.notice = notice;
    }
    pub fn running(&self) -> bool {
        self.job.is_some()
    }
    pub fn submit(&mut self, resolve: Resolver, request: Request) -> Result<(), String> {
        if self.job.is_some() {
            return Err("Project read busy; draft retained".into());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let stopped = cancel.clone();
        let retained = request.clone();
        let notice = self.notice.clone();
        let handle = std::thread::Builder::new()
            .name("asura-project-page-client".into())
            .spawn(move || {
                let _settlement = notice.guard();
                let limit = match request.read {
                    ProjectPage::Only(_) => 1,
                    _ => crate::conversation::PAGE,
                };
                let run = || -> Result<Page, String> {
                    let runtime = resolve(false).map_err(|e| e.to_string())?;
                    let mut client = asura_client::Client::attach(
                        &runtime,
                        concat!("asura/", env!("CARGO_PKG_VERSION")),
                        &|| stopped.load(Ordering::Acquire),
                    )
                    .map_err(|e| e.to_string())?;
                    let read_stop = stopped.clone();
                    client.set_cancel(Arc::new(move || read_stop.load(Ordering::Acquire)));
                    crate::conversation::project_page(&mut client, request.read, limit)
                };
                let result = run();
                Update { request, result }
            })
            .map_err(|e| e.to_string())?;
        self.job = Some(Job {
            handle,
            request: retained,
            cancel,
        });
        Ok(())
    }
    pub fn poll(&mut self) -> Option<Update> {
        if !self.job.as_ref()?.handle.is_finished() {
            return None;
        }
        let job = self.job.take().unwrap();
        Some(job.handle.join().unwrap_or_else(|_| Update {
            request: job.request,
            result: Err("Project read failed".into()),
        }))
    }
    pub fn cancel(&self) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Release);
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        // Exit stops the read; a read changes nothing, so the thread ends
        // on its own once its wait sees the cancel.
        self.cancel();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unavailable_service_settles_the_read_with_an_error_and_frees_the_slot() {
        let mut worker = Worker::default();
        let resolve: Resolver = |_| Err(asura_platform::Error::Unavailable);
        let request = Request {
            read: ProjectPage::Only([1; 16]),
            purpose: Purpose::Select {
                draft: "/project select x".into(),
            },
        };
        worker.submit(resolve, request.clone()).unwrap();
        assert!(worker.submit(resolve, request).is_err());
        let update = loop {
            if let Some(update) = worker.poll() {
                break update;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        assert!(update.result.is_err());
        assert!(!worker.running());
    }
}
