//! Isolated ticket fixtures. This feature is enabled only by test dependencies.
//! These constructors never access a live writer, journal, database or owner claim.
use super::{
    Arc, AtomicBool, AtomicUsize, Command, Job, JobSettlement, Ordering, Receiver, Reply, Result,
    Room, SyncSender, Ticket, WriterClient, mpsc,
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
pub fn controlled_ticket(mutation: bool) -> (Ticket, Controller) {
    let (reply, receiver) = mpsc::sync_channel(1);
    let settled = Arc::new(AtomicBool::new(false));
    let ticket = Ticket {
        receiver,
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
            room: Arc::default(),
        },
    };
    (ticket, controller)
}

/// The far side of a detached writer queue: no journal worker, no owner
/// claim. The test answers each queued job in order.
pub struct QueueFixture {
    receiver: Receiver<Option<Job>>,
    closing: Arc<AtomicBool>,
    room: Arc<Room>,
}
impl QueueFixture {
    /// Blocks until the next queued job, replies with `answer(command)` and
    /// settles the job, releasing its slot. False when the queue closed.
    pub fn answer_next(&self, answer: impl FnOnce(Command) -> Result<Reply>) -> bool {
        let Ok(Some(job)) = self.receiver.recv() else {
            return false;
        };
        let _ = job.reply.try_send(answer(job.command));
        drop(job.settlement);
        true
    }
    /// Closes the queue as `WriterHandle::close` does.
    pub fn close(&self) {
        self.closing.store(true, Ordering::Release);
        self.room.notify();
    }
}

/// A `WriterClient` with the real 8-slot admission rules whose jobs only the
/// returned fixture answers.
pub fn detached_queue() -> (WriterClient, QueueFixture) {
    let (sender, receiver) = mpsc::sync_channel(super::QUEUE_SLOTS);
    let closing = Arc::new(AtomicBool::new(false));
    let room = Arc::<Room>::default();
    (
        WriterClient {
            sender,
            count: Arc::new(AtomicUsize::new(0)),
            closing: closing.clone(),
            wake: None,
            room: room.clone(),
        },
        QueueFixture {
            receiver,
            closing,
            room,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::writer::Error;
    #[test]
    fn response_and_settlement_are_independent_using_the_real_guard() {
        let (mut ticket, controller) = controlled_ticket(false);
        assert!(ticket.poll().is_none());
        assert!(!ticket.is_settled());
        assert!(controller.publish(Err(Error::Unavailable)));
        assert!(matches!(ticket.poll(), Some(Err(Error::Unavailable))));
        assert!(!ticket.is_settled());
        controller.finish();
        assert!(ticket.is_settled());
    }
}
