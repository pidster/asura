//! Isolated ticket fixtures. This feature is enabled only by test dependencies.
//! These constructors never access a live writer, journal, database or owner claim.
use super::{
    Arc, AtomicBool, AtomicUsize, Instant, JobSettlement, Reply, Result, SyncSender, Ticket, mpsc,
};

/// Holds a synthetic job's actual settlement guard, independently of its response.
pub struct Controller {
    reply: SyncSender<Result<Reply>>,
    _settlement: JobSettlement,
}
impl Controller {
    /// Publish one bounded reply without claiming that the job has settled.
    pub fn publish(&self, result: Result<Reply>) -> bool {
        self.reply.try_send(result).is_ok()
    }
    /// End the synthetic job through the same guard used by real accepted jobs.
    pub fn finish(self) {
        drop(self);
    }
}

/// Construct a pending ticket with a separately controlled response and settlement.
/// Dropping the controller, including unwinding, always settles the synthetic job.
pub fn controlled_ticket(deadline: Instant, mutation: bool) -> (Ticket, Controller) {
    let (reply, receiver) = mpsc::sync_channel(1);
    let settled = Arc::new(AtomicBool::new(false));
    let ticket = Ticket {
        receiver,
        deadline,
        mutation,
        done: false,
        settled: settled.clone(),
    };
    let controller = Controller {
        reply,
        _settlement: JobSettlement {
            settled,
            count: Arc::new(AtomicUsize::new(1)),
            wake: None,
        },
    };
    (ticket, controller)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::writer::Error;
    #[test]
    fn response_and_settlement_are_independent_using_the_real_guard() {
        let (mut ticket, controller) = controlled_ticket(Instant::now(), false);
        assert!(matches!(ticket.poll(), Some(Err(Error::Deadline))));
        assert!(!ticket.is_settled());
        assert!(controller.publish(Err(Error::Unavailable)));
        assert!(ticket.poll().is_none());
        assert!(!ticket.is_settled());
        controller.finish();
        assert!(ticket.is_settled());
    }
}
