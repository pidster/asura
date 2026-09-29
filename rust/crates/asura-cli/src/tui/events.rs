//! Typed readiness routing; payload ownership remains with bounded worker mailboxes.
use asura_platform::events::{self, Inbox, Priority, Rejection, WakeSender};
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use std::{
    io,
    os::fd::{AsRawFd, RawFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
#[derive(Clone, Copy)]
pub(super) enum Source {
    Config = 1,
    Conversation = 2,
    Queue = 4,
    Context = 8,
    Status = 16,
    QueueWatch = 32,
    Models = 64,
    Audit = 128,
}
#[derive(Default)]
struct Signals {
    ready: AtomicU8,
    settling: AtomicU8,
}
#[derive(Clone, Default)]
pub(super) struct Notice(Option<(Arc<Signals>, WakeSender, Source)>);
impl Notice {
    pub fn ready(&self) {
        if let Some((signals, wake, source)) = &self.0 {
            signals.ready.fetch_or(*source as u8, Ordering::Release);
            let _ = wake.notify();
        }
    }
    pub fn settled(&self) {
        if let Some((signals, _, source)) = &self.0 {
            signals
                .settling
                .fetch_and(!(*source as u8), Ordering::AcqRel);
        }
    }
    pub fn guard(&self) -> Settlement {
        Settlement(self.clone())
    }
}
pub(super) struct Settlement(Notice);
impl Drop for Settlement {
    fn drop(&mut self) {
        if let Some((signals, _, source)) = &self.0.0 {
            signals.settling.fetch_or(*source as u8, Ordering::Release);
        }
        self.0.ready();
    }
}
#[derive(Debug)]
pub(super) enum Input {
    Event(Event),
    PasteReady,
    PasteRejected,
}
const MAX_RAW_PASTE: usize = 3 * super::editor::MAX_DRAFT_BYTES;
pub(super) struct Router {
    inbox: Inbox<Input>,
    paste: mpsc::Receiver<Box<str>>,
    paste_pending: Arc<AtomicBool>,
    signals: Arc<Signals>,
    wake: WakeSender,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<io::Error>>>,
    input: Option<JoinHandle<()>>,
}
struct ReaderExit {
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<io::Error>>>,
    wake: WakeSender,
}
impl Drop for ReaderExit {
    fn drop(&mut self) {
        if !self.stop.load(Ordering::Acquire) {
            self.error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_or_insert_with(|| io::Error::other("Terminal reader stopped"));
        }
        let _ = self.wake.notify();
    }
}
fn input_priority(value: &Event) -> Priority {
    match value {
        Event::Key(key)
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c' | 'd')) =>
        {
            Priority::Critical
        }
        _ => Priority::Interaction,
    }
}
fn prepare_input(
    value: Event,
    slot: &mpsc::SyncSender<Box<str>>,
    pending: &AtomicBool,
) -> io::Result<Input> {
    Ok(match value {
        Event::Paste(text) if text.len() <= MAX_RAW_PASTE => {
            pending.store(true, Ordering::Release);
            slot.try_send(text.into_boxed_str())
                .map_err(|_| io::Error::other("Paste slot ownership violated"))?;
            Input::PasteReady
        }
        Event::Paste(_) => Input::PasteRejected,
        value => Input::Event(value),
    })
}
fn publish_input(
    sender: &events::Sender<Input>,
    mut value: Input,
    priority: Priority,
    resize: bool,
    stop: &AtomicBool,
) -> io::Result<()> {
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let size = std::mem::size_of::<Input>();
        let admission = if resize {
            sender.latest(0, value, size)
        } else {
            sender.try_send(priority, value, size)
        };
        match admission {
            Ok(()) => return Ok(()),
            Err(rejected) if matches!(rejected.reason, Rejection::Contended | Rejection::Full) => {
                value = rejected.event;
                // Capacity/cancellation unpark tokens survive a notification before park.
                std::thread::park();
            }
            Err(rejected) => {
                return Err(io::Error::other(format!(
                    "Terminal input admission failed: {:?}",
                    rejected.reason
                )));
            }
        }
    }
}
impl Router {
    pub fn take_paste(&self) -> io::Result<Box<str>> {
        self.paste
            .try_recv()
            .map_err(|_| io::Error::other("Paste readiness without payload"))
    }
    pub fn paste_consumed(&self) {
        self.paste_pending.store(false, Ordering::Release);
        if let Some(input) = &self.input {
            input.thread().unpark();
        }
    }
    pub fn start() -> io::Result<Self> {
        let (inbox, sender) = events::inbox()?;
        let wake = sender.wake();
        let (paste_sender, paste) = mpsc::sync_channel(1);
        let paste_pending = Arc::new(AtomicBool::new(false));
        let held_paste = paste_pending.clone();
        let signals = Arc::new(Signals::default());
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let token = stop.clone();
        let failed = error.clone();
        let signal = wake.clone();
        let input = std::thread::Builder::new()
            .name("asura-terminal-input".into())
            .spawn(move || {
                let _exit = ReaderExit {
                    stop: token.clone(),
                    error: failed.clone(),
                    wake: signal.clone(),
                };
                let result = (|| -> io::Result<()> {
                    while !token.load(Ordering::Acquire) {
                        if held_paste.load(Ordering::Acquire) {
                            std::thread::park();
                            continue;
                        }
                        if !event::poll(Duration::from_millis(100))? {
                            continue;
                        }
                        let value = event::read()?;
                        let priority = input_priority(&value);
                        let resize = matches!(value, Event::Resize(..));
                        let value = prepare_input(value, &paste_sender, &held_paste)?;
                        publish_input(&sender, value, priority, resize, &token)?;
                    }
                    Ok(())
                })();
                if let Err(problem) = result {
                    *failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(problem);
                }
                let _ = signal.notify();
            })?;
        Ok(Self {
            inbox,
            paste,
            paste_pending,
            signals,
            wake,
            stop,
            error,
            input: Some(input),
        })
    }
    pub fn notice(&self, source: Source) -> Notice {
        Notice(Some((self.signals.clone(), self.wake.clone(), source)))
    }
    pub fn input(&self) -> io::Result<Vec<Input>> {
        let values = self.inbox.drain(32)?;
        if let Some(input) = &self.input {
            input.thread().unpark();
        }
        match self.error.try_lock() {
            Ok(mut error) => {
                if let Some(error) = error.take() {
                    return Err(error);
                }
            }
            Err(std::sync::TryLockError::Poisoned(error)) => {
                if let Some(error) = error.into_inner().take() {
                    return Err(error);
                }
            }
            Err(_) => {
                let _ = self.wake.notify();
            }
        }
        Ok(values)
    }
    pub fn ready(&self, settlement_due: bool) -> u8 {
        self.signals.ready.swap(0, Ordering::AcqRel)
            | if settlement_due {
                self.signals.settling.load(Ordering::Acquire)
            } else {
                0
            }
    }
    pub fn settled(&self, source: Source) {
        self.signals
            .settling
            .fetch_and(!(source as u8), Ordering::AcqRel);
    }
    pub fn has_settling(&self) -> bool {
        self.signals.settling.load(Ordering::Acquire) != 0
    }
    pub fn fd(&self) -> RawFd {
        self.inbox.as_raw_fd()
    }
    pub fn cancel(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(input) = &self.input {
            input.thread().unpark();
        }
    }
}
impl Drop for Router {
    fn drop(&mut self) {
        self.cancel();
        let end = Instant::now() + Duration::from_millis(150);
        if let Some(handle) = self.input.take() {
            while !handle.is_finished() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(2));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
            // No replacement reader: process exit contains a stalled parser.
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paste_slot_preserves_editor_and_capture_limits_without_large_inbox_payloads() {
        let (send, receive) = mpsc::sync_channel(1);
        let pending = AtomicBool::new(false);
        for length in [32768, super::super::editor::MAX_DRAFT_BYTES, MAX_RAW_PASTE] {
            assert!(matches!(
                prepare_input(Event::Paste("x".repeat(length)), &send, &pending).unwrap(),
                Input::PasteReady
            ));
            assert!(pending.load(Ordering::Acquire));
            assert_eq!(receive.try_recv().unwrap().len(), length);
            pending.store(false, Ordering::Release);
        }
        assert!(matches!(
            prepare_input(Event::Paste("x".repeat(MAX_RAW_PASTE + 1)), &send, &pending).unwrap(),
            Input::PasteRejected
        ));
        assert!(receive.try_recv().is_err());
        assert!(!pending.load(Ordering::Acquire));
        assert!(matches!(
            prepare_input(Event::FocusGained, &send, &pending).unwrap(),
            Input::Event(Event::FocusGained)
        ));
    }
    #[test]
    fn full_input_retains_event_until_capacity_and_cancel_unparks() {
        for cancel in [false, true] {
            let (inbox, sender) = events::inbox().unwrap();
            for _ in 0..128 {
                sender
                    .try_send(
                        Priority::Interaction,
                        Input::Event(Event::FocusGained),
                        std::mem::size_of::<Input>(),
                    )
                    .unwrap();
            }
            let stop = Arc::new(AtomicBool::new(false));
            let token = stop.clone();
            let (done, result) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let outcome = publish_input(
                    &sender,
                    Input::Event(Event::FocusLost),
                    Priority::Interaction,
                    false,
                    &token,
                );
                done.send(outcome).unwrap();
            });
            if cancel {
                stop.store(true, Ordering::Release);
            } else {
                assert_eq!(inbox.drain(1).unwrap().len(), 1);
            }
            worker.thread().unpark();
            result
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
            worker.join().unwrap();
            let mut found = 0;
            for _ in 0..3 {
                found += inbox
                    .drain(64)
                    .unwrap()
                    .into_iter()
                    .filter(|event| matches!(event, Input::Event(Event::FocusLost)))
                    .count();
            }
            assert_eq!(found, usize::from(!cancel));
        }
    }
    #[test]
    fn notices_coalesce_and_settlement_is_distinct() {
        let (_reader, wake) = events::wake_pair().unwrap();
        let signals = Arc::new(Signals::default());
        let notice = Notice(Some((signals.clone(), wake, Source::Queue)));
        notice.ready();
        notice.ready();
        assert_eq!(signals.ready.swap(0, Ordering::AcqRel), Source::Queue as u8);
        assert_eq!(signals.settling.load(Ordering::Acquire), 0);
        drop(notice.guard());
        assert_eq!(signals.ready.load(Ordering::Acquire), Source::Queue as u8);
        assert_eq!(
            signals.settling.load(Ordering::Acquire),
            Source::Queue as u8
        );
    }
    #[test]
    fn shared_conversation_readiness_drains_both_independent_workers() {
        let (reader, wake) = events::wake_pair().unwrap();
        let signals = Arc::new(Signals::default());
        let conversation = Notice(Some((signals.clone(), wake.clone(), Source::Conversation)));
        let project_admin = Notice(Some((signals.clone(), wake, Source::Conversation)));
        let conversation_result = Arc::new(Mutex::new(None));
        let project_result = Arc::new(Mutex::new(None));
        let c = conversation_result.clone();
        let p = project_result.clone();
        let first = std::thread::spawn(move || {
            let _settlement = conversation.guard();
            *c.lock().unwrap() = Some("conversation");
            conversation.ready();
        });
        let second = std::thread::spawn(move || {
            let _settlement = project_admin.guard();
            *p.lock().unwrap() = Some("project");
            project_admin.ready();
        });
        first.join().unwrap();
        second.join().unwrap();
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
        reader.drain().unwrap();
        let bits = signals.ready.swap(0, Ordering::AcqRel);
        if bits & Source::Conversation as u8 != 0 {
            assert_eq!(
                conversation_result.lock().unwrap().take(),
                Some("conversation")
            );
            assert_eq!(project_result.lock().unwrap().take(), Some("project"));
        } else {
            panic!("shared readiness lost");
        }
    }
    #[test]
    fn completion_wakes_descriptor_even_with_full_telemetry_and_input() {
        let (inbox, sender) = events::inbox().unwrap();
        for key in 0..8 {
            sender
                .latest(key, Event::Resize(80, 24), std::mem::size_of::<Event>())
                .unwrap();
        }
        for _ in 0..128 {
            sender
                .try_send(
                    Priority::Interaction,
                    Event::FocusGained,
                    std::mem::size_of::<Event>(),
                )
                .unwrap();
        }
        let signals = Arc::new(Signals::default());
        let notice = Notice(Some((signals.clone(), sender.wake(), Source::Config)));
        let worker = std::thread::spawn(move || {
            notice.ready();
        });
        let ready = asura_platform::poll(
            &[asura_platform::PollInterest {
                fd: inbox.as_raw_fd(),
                read: true,
                write: false,
            }],
            Duration::from_secs(1),
        )
        .unwrap();
        worker.join().unwrap();
        assert!(!ready.is_empty());
        assert_eq!(signals.ready.load(Ordering::Acquire), Source::Config as u8);
        sender
            .try_send(
                Priority::Critical,
                Event::FocusLost,
                std::mem::size_of::<Event>(),
            )
            .unwrap();
        assert_eq!(inbox.drain(1).unwrap(), vec![Event::FocusLost]);
    }
    #[test]
    fn publication_without_input_wakes_an_idle_dispatcher() {
        let (reader, wake) = events::wake_pair().unwrap();
        let signals = Arc::new(Signals::default());
        let notice = Notice(Some((signals.clone(), wake, Source::Context)));
        let worker = std::thread::spawn(move || notice.ready());
        let ready = asura_platform::poll(
            &[asura_platform::PollInterest {
                fd: reader.as_raw_fd(),
                read: true,
                write: false,
            }],
            Duration::from_secs(1),
        )
        .unwrap();
        worker.join().unwrap();
        assert!(!ready.is_empty());
        reader.drain().unwrap();
        assert_eq!(
            signals.ready.swap(0, Ordering::AcqRel),
            Source::Context as u8
        );
    }
    #[test]
    fn interrupt_is_critical_and_paste_accounts_dynamic_storage() {
        use crossterm::event::KeyEvent;
        assert_eq!(
            input_priority(&Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL
            ))),
            Priority::Critical
        );
        assert_eq!(
            input_priority(&Event::Paste("text".into())),
            Priority::Interaction
        );
        assert_eq!(MAX_RAW_PASTE, 196608);
        assert!(std::mem::size_of::<Input>() < 32768);
    }
}
