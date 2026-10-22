//! Text pages over retained payloads and the page-read pool. A page read
//! waits for its reply with no time bound (owner, 2026-10-21, WB1); J3 and
//! J4 add the model and client callers.
use super::store::{Location, PayloadId, PayloadState, PayloadStore};
use crate::authority::conversation::{Hash, Id};
use asura_platform::payload_io::PayloadError;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, Weak};

/// Source bytes per page.
pub const PAGE_LIMIT: usize = 8192;
/// Bytes of delivered page text: the reader stops early so that the text,
/// with each invalid sequence replaced by U+FFFD, never exceeds it (J3
/// page block).
pub const TEXT_LIMIT: usize = 8192;
/// Queued page requests; the next one is `busy`.
pub const PAGE_QUEUE: usize = 8;

/// Page states, as the composer interface lists them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageState {
    Live,
    Complete,
    Incomplete,
    UnverifiedPrefix,
    Expired,
    Missing,
    Corrupt,
    /// The person deleted the job's output (J4).
    Deleted,
    Busy,
    Denied,
    StaleCursor,
}
impl PageState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Complete => "complete",
            Self::Incomplete => "incomplete",
            Self::UnverifiedPrefix => "unverified_prefix",
            Self::Expired => "expired",
            Self::Missing => "missing",
            Self::Corrupt => "corrupt",
            Self::Deleted => "deleted",
            Self::Busy => "busy",
            Self::Denied => "denied",
            Self::StaleCursor => "stale_cursor",
        }
    }
}

/// One page request. `scope` binds the request to the operation that owns
/// the payload; another operation is `denied`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageRequest {
    pub payload: PayloadId,
    pub offset: u64,
    pub limit: usize,
    pub scope: Option<Id>,
}

/// One page. `start` is the actual start after snapping forward to a UTF-8
/// boundary; `next_offset` is the source offset after the page. Invalid
/// UTF-8 is replaced with U+FFFD at read (R5, SD6).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Page {
    pub state: PageState,
    pub start: u64,
    pub text: String,
    pub next_offset: u64,
    pub total: u64,
    /// The digest of a settled payload.
    pub digest: Option<Hash>,
    /// The watermark of a live payload.
    pub watermark: Option<u64>,
}
impl Page {
    fn empty(state: PageState, offset: u64) -> Self {
        Self {
            state,
            start: offset,
            text: String::new(),
            next_offset: offset,
            total: 0,
            digest: None,
            watermark: None,
        }
    }
}

fn continuation(byte: u8) -> bool {
    byte & 0xC0 == 0x80
}
fn scalar_len(lead: u8) -> usize {
    match lead {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}
/// The length of `bytes` without a trailing scalar that is still incomplete:
/// a lead byte followed by fewer continuation bytes than it announces.
fn without_open_scalar(bytes: &[u8]) -> usize {
    let len = bytes.len();
    let mut lead = len;
    while lead > 0 && len - lead < 3 && continuation(bytes[lead - 1]) {
        lead -= 1;
    }
    if lead > 0 {
        let start = lead - 1;
        if !continuation(bytes[start]) && start + scalar_len(bytes[start]) > len {
            return start;
        }
    }
    len
}
/// Snaps a window of source bytes. `bytes` starts at the requested offset
/// and may extend up to 4 bytes past the limit. Returns the skipped prefix
/// length and the page length. The start moves forward past at most three
/// continuation bytes; the end trims back so a scalar is never split; a page
/// that would be empty keeps its first whole scalar.
///
/// `open_end` marks a window that ends at a live payload's watermark: more
/// bytes may follow, so a scalar still incomplete there is left for the next
/// page instead of becoming U+FFFD, even if the page becomes empty.
pub(crate) fn snap(bytes: &[u8], limit: usize, open_end: bool) -> (usize, usize) {
    let skip = bytes
        .iter()
        .take(3)
        .take_while(|b| continuation(**b))
        .count();
    let window = &bytes[skip..];
    let mut end = limit.min(window.len());
    if end < window.len() && continuation(window[end]) {
        let mut lead = end;
        while lead > 0 && end - lead < 3 && continuation(window[lead]) {
            lead -= 1;
        }
        if !continuation(window[lead]) && lead + scalar_len(window[lead]) > end {
            end = lead;
        }
    }
    if open_end && end == window.len() {
        return (skip, without_open_scalar(window));
    }
    if end == 0 && !window.is_empty() {
        end = scalar_len(window[0]).min(window.len());
    }
    (skip, end)
}

/// The longest prefix of `bytes` whose text, with each maximal invalid
/// sequence replaced by U+FFFD as `String::from_utf8_lossy` does, is at
/// most `max_text` bytes: the text and the source bytes it consumed. A
/// non-empty input always yields its first scalar or replacement, which is
/// at most 4 bytes of text.
pub(crate) fn bounded_text(bytes: &[u8], max_text: usize) -> (String, usize) {
    const REPLACEMENT: char = char::REPLACEMENT_CHARACTER;
    let mut text = String::with_capacity(bytes.len().min(max_text));
    let mut consumed = 0;
    for chunk in bytes.utf8_chunks() {
        for scalar in chunk.valid().chars() {
            if text.len() + scalar.len_utf8() > max_text && consumed > 0 {
                return (text, consumed);
            }
            text.push(scalar);
            consumed += scalar.len_utf8();
        }
        if !chunk.invalid().is_empty() {
            if text.len() + REPLACEMENT.len_utf8() > max_text && consumed > 0 {
                return (text, consumed);
            }
            text.push(REPLACEMENT);
            consumed += chunk.invalid().len();
        }
    }
    (text, consumed)
}

/// Blocking: reads one text page. Page-read worker only.
pub fn read_page(store: &PayloadStore, request: PageRequest) -> Page {
    if store.deleted(request.payload) {
        return Page::empty(PageState::Deleted, request.offset);
    }
    if store.was_expired(request.payload) {
        return Page::empty(PageState::Expired, request.offset);
    }
    let Some(entry) = store.hold_payload(request.payload) else {
        return Page::empty(PageState::Missing, request.offset);
    };
    let page = read_held(store, request, &entry);
    store.release_payload(request.payload);
    page
}
fn read_held(
    store: &PayloadStore,
    request: PageRequest,
    entry: &super::store::PayloadEntry,
) -> Page {
    if request.scope.is_some_and(|scope| scope != entry.operation) {
        return Page::empty(PageState::Denied, request.offset);
    }
    let state = match entry.state {
        PayloadState::Live => PageState::Live,
        PayloadState::Complete => PageState::Complete,
        PayloadState::Incomplete => PageState::Incomplete,
        PayloadState::UnverifiedPrefix => PageState::UnverifiedPrefix,
        PayloadState::Corrupt => return Page::empty(PageState::Corrupt, request.offset),
        PayloadState::Missing => return Page::empty(PageState::Missing, request.offset),
    };
    let watermark = entry.watermark.as_ref().map(|w| w.load(Ordering::Acquire));
    let total = watermark.unwrap_or(entry.total);
    let mut page = Page {
        state,
        start: request.offset,
        text: String::new(),
        next_offset: request.offset,
        total,
        digest: entry.digest,
        watermark: if state == PageState::Live {
            watermark
        } else {
            None
        },
    };
    if request.offset > total {
        page.state = PageState::StaleCursor;
        page.next_offset = total;
        return page;
    }
    if request.offset == total {
        return page;
    }
    let limit = request.limit.clamp(1, PAGE_LIMIT);
    let location = if entry.location == Location::None {
        Location::Staging
    } else {
        entry.location
    };
    let file = match store.open_read(request.payload, location) {
        Ok(file) => file,
        Err(PayloadError::Absent) => return Page::empty(PageState::Missing, request.offset),
        Err(PayloadError::Unsafe) => return Page::empty(PageState::Corrupt, request.offset),
        Err(_) => return Page::empty(PageState::Busy, request.offset),
    };
    // Never read past the watermark (live) or the recorded total.
    let window = (total - request.offset).min(limit as u64 + 4) as usize;
    let mut buffer = vec![0u8; window];
    let mut filled = 0;
    while filled < window {
        match file.read_at(request.offset + filled as u64, &mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return Page::empty(PageState::Busy, request.offset),
        }
    }
    buffer.truncate(filled);
    // A page that reaches a watermark (live, or an `unconfirmed` prefix still
    // being written) may end inside a scalar whose remaining bytes are not
    // written yet.
    let open_end = watermark.is_some() && request.offset + filled as u64 >= total;
    let (skip, length) = snap(&buffer, limit, open_end);
    // The delivered text never exceeds 8,192 bytes; the next offset records
    // the source bytes the text consumed.
    let (text, consumed) = bounded_text(&buffer[skip..skip + length], TEXT_LIMIT);
    page.start = request.offset + skip as u64;
    page.text = text;
    page.next_offset = page.start + consumed as u64;
    page
}

/// Who asked for a page. One worker is reserved for model requests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Origin {
    Model,
    Client,
}

/// The answer to a submitted page read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PageReply {
    Page(Page),
    /// The pool shut down before the read replied.
    Cancelled,
}

/// One request's reply slot, shared by its ticket and the worker.
#[derive(Debug)]
enum Reply {
    /// Queued or on a worker; the ticket waits.
    Waiting,
    Ready(PageReply),
    /// The ticket took the reply.
    Taken,
    /// The caller cancelled or detached; a later reply is discarded.
    Withdrawn,
}
struct Slot {
    reply: Mutex<Reply>,
    ready: Condvar,
}
impl Slot {
    fn new() -> Self {
        Self {
            reply: Mutex::new(Reply::Waiting),
            ready: Condvar::new(),
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Reply> {
        self.reply.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Delivers a reply to a waiting ticket; any other state discards it.
    fn deliver(&self, reply: PageReply) {
        let mut state = self.lock();
        if matches!(*state, Reply::Waiting) {
            *state = Reply::Ready(reply);
            drop(state);
            self.ready.notify_all();
        }
    }
    fn withdrawn(&self) -> bool {
        matches!(*self.lock(), Reply::Withdrawn)
    }
}
struct Job {
    request: PageRequest,
    origin: Origin,
    slot: Arc<Slot>,
}
#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    /// Slots of the requests on a worker, so shutdown can end their tickets.
    running: Vec<Arc<Slot>>,
    closed: bool,
    held_for_test: bool,
}
struct Shared {
    store: Arc<PayloadStore>,
    queue: Mutex<Queue>,
    work: Condvar,
}
fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Queue> {
    shared.queue.lock().unwrap_or_else(|e| e.into_inner())
}

/// A submitted page read. It waits for its reply with no time bound
/// (owner, WB1). Closures: the reply, the caller's cancel or detach
/// ([`Ticket::cancel`], or dropping the ticket), and pool shutdown, which
/// answers [`PageReply::Cancelled`].
pub struct Ticket {
    slot: Arc<Slot>,
    pool: Weak<Shared>,
}
impl Ticket {
    /// Nonblocking: the reply once it has arrived. It is returned once.
    pub fn poll(&self) -> Option<PageReply> {
        let mut state = self.slot.lock();
        match std::mem::replace(&mut *state, Reply::Taken) {
            Reply::Ready(reply) => Some(reply),
            other => {
                *state = other;
                None
            }
        }
    }
    /// Blocking: waits for the reply with no time bound. Worker threads
    /// only, never the reactor. A reply already taken by [`Self::poll`]
    /// answers `Cancelled`.
    pub fn wait(self) -> PageReply {
        let mut state = self.slot.lock();
        loop {
            match std::mem::replace(&mut *state, Reply::Taken) {
                Reply::Ready(reply) => return reply,
                Reply::Waiting => {
                    *state = Reply::Waiting;
                    state = self
                        .slot
                        .ready
                        .wait(state)
                        .unwrap_or_else(|e| e.into_inner());
                }
                Reply::Taken | Reply::Withdrawn => return PageReply::Cancelled,
            }
        }
    }
    /// The caller's cancel: withdraws a queued request at once, freeing its
    /// queue place. A request already on a worker continues; its reply is
    /// discarded and the worker stays held until the read returns.
    pub fn cancel(self) {}
    fn withdraw(&self) {
        // Lock order: queue, then slot (as the workers and shutdown).
        let pool = self.pool.upgrade();
        let mut queue = pool.as_deref().map(lock);
        if let Some(queue) = queue.as_mut() {
            queue.jobs.retain(|job| !Arc::ptr_eq(&job.slot, &self.slot));
        }
        let mut state = self.slot.lock();
        if matches!(*state, Reply::Waiting | Reply::Ready(_)) {
            *state = Reply::Withdrawn;
        }
    }
}
impl Drop for Ticket {
    /// Detach: the same as [`Ticket::cancel`].
    fn drop(&mut self) {
        self.withdraw();
    }
}

/// Two retained page-read workers, one reserved for model requests, and a
/// queue of 8 (J2 numeric limits). No replacement worker ever starts, and no
/// time bound ends a read.
pub struct PagePool {
    shared: Arc<Shared>,
}
impl PagePool {
    /// Starts both workers. Fails only if a thread cannot start.
    pub fn start(store: Arc<PayloadStore>) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            store,
            queue: Mutex::new(Queue::default()),
            work: Condvar::new(),
        });
        let pool = Self {
            shared: shared.clone(),
        };
        for (index, model_only) in [(0, false), (1, true)] {
            let shared = shared.clone();
            // A failed start drops `pool`, which closes the started worker.
            std::thread::Builder::new()
                .name(format!("asura-page-read-{index}"))
                .spawn(move || worker(&shared, model_only))?;
        }
        Ok(pool)
    }
    /// Queues a request; the ninth queued request is `busy`, as is any
    /// request after shutdown.
    pub fn submit(&self, request: PageRequest, origin: Origin) -> Result<Ticket, PageState> {
        let mut queue = lock(&self.shared);
        if queue.closed || queue.jobs.len() >= PAGE_QUEUE {
            return Err(PageState::Busy);
        }
        let slot = Arc::new(Slot::new());
        queue.jobs.push_back(Job {
            request,
            origin,
            slot: slot.clone(),
        });
        drop(queue);
        self.shared.work.notify_all();
        Ok(Ticket {
            slot,
            pool: Arc::downgrade(&self.shared),
        })
    }
    /// Queued requests not yet taken by a worker.
    pub fn queued(&self) -> usize {
        lock(&self.shared).jobs.len()
    }
    /// Requests on a worker, including withdrawn ones still reading.
    pub fn running(&self) -> usize {
        lock(&self.shared).running.len()
    }
    /// Test hook: workers hold each taken request until released.
    #[cfg(any(test, feature = "test-support"))]
    pub fn hold_for_test(&self, held: bool) {
        lock(&self.shared).held_for_test = held;
        self.shared.work.notify_all();
    }
    /// Shutdown: stops new requests and ends every waiting ticket, queued or
    /// on a worker, with `Cancelled`. Workers exit after their current read.
    pub fn close(&self) {
        let mut queue = lock(&self.shared);
        queue.closed = true;
        let queued: Vec<Job> = queue.jobs.drain(..).collect();
        let running = queue.running.clone();
        drop(queue);
        self.shared.work.notify_all();
        for slot in queued.iter().map(|job| &job.slot).chain(running.iter()) {
            slot.deliver(PageReply::Cancelled);
        }
    }
}
impl Drop for PagePool {
    fn drop(&mut self) {
        self.close();
    }
}
fn worker(shared: &Shared, model_only: bool) {
    loop {
        let job = {
            let mut queue = lock(shared);
            loop {
                if queue.closed {
                    return;
                }
                let position = queue
                    .jobs
                    .iter()
                    .position(|job| !model_only || job.origin == Origin::Model);
                if let Some(position) = position {
                    let job = queue.jobs.remove(position).expect("queued job");
                    queue.running.push(job.slot.clone());
                    break job;
                }
                queue = shared.work.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
        };
        // Test hold: the request stays on this worker until released or
        // shutdown; each changes the flag and wakes the workers.
        {
            let mut queue = lock(shared);
            while queue.held_for_test && !queue.closed {
                queue = shared.work.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
        }
        // A request withdrawn before its read began is skipped.
        if !job.slot.withdrawn() {
            let page = read_page(&shared.store, job.request);
            job.slot.deliver(PageReply::Page(page));
        }
        lock(shared)
            .running
            .retain(|slot| !Arc::ptr_eq(slot, &job.slot));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ticket() -> Ticket {
        Ticket {
            slot: Arc::new(Slot::new()),
            pool: Weak::new(),
        }
    }
    fn page(text: &str) -> PageReply {
        let mut page = Page::empty(PageState::Complete, 0);
        page.text = text.into();
        PageReply::Page(page)
    }
    /// J2T-16 (unit): a ticket waits for its reply with no time bound and
    /// takes it once; a reply after withdrawal is discarded.
    #[test]
    fn j2t_16_ticket_waits_for_its_reply_and_discards_after_withdrawal() {
        let waiting = ticket();
        assert!(waiting.poll().is_none());
        let slot = waiting.slot.clone();
        let waiter = std::thread::spawn(move || waiting.wait());
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!waiter.is_finished(), "no step ends the wait");
        slot.deliver(page("a"));
        assert_eq!(waiter.join().unwrap(), page("a"));
        // A second reply to a taken slot is discarded.
        slot.deliver(page("b"));
        assert!(matches!(*slot.lock(), Reply::Taken));
        // Poll takes the reply once.
        let polled = ticket();
        polled.slot.deliver(page("c"));
        assert_eq!(polled.poll(), Some(page("c")));
        assert_eq!(polled.poll(), None);
        // Withdrawal (cancel or detach, with or without a live pool).
        let detached = ticket();
        let slot = detached.slot.clone();
        detached.cancel();
        assert!(slot.withdrawn());
        slot.deliver(page("d"));
        assert!(slot.withdrawn(), "the late reply is discarded");
        // Shutdown's reply reaches a waiting ticket.
        let closing = ticket();
        closing.slot.deliver(PageReply::Cancelled);
        assert_eq!(closing.wait(), PageReply::Cancelled);
    }
    #[test]
    fn snapping_moves_the_start_forward_and_trims_the_end_back() {
        let text = "aé€😀b".as_bytes(); // 1 + 2 + 3 + 4 + 1 bytes
        // Offsets inside the 2-, 3- and 4-byte scalars move forward.
        assert_eq!(snap(&text[2..], 8, false), (1, 8));
        assert_eq!(snap(&text[4..], 8, false), (2, 5));
        assert_eq!(snap(&text[7..], 8, false), (3, 1));
        // An end inside a scalar trims back to its lead byte.
        assert_eq!(snap(text, 2, false), (0, 1));
        assert_eq!(snap(text, 5, false), (0, 3));
        assert_eq!(snap(text, 8, false), (0, 6));
        assert_eq!(snap(text, 10, false), (0, 10));
        // A page that would be empty keeps its first whole scalar.
        assert_eq!(snap(&text[6..], 1, false), (0, 4));
        // Invalid bytes are not snapped past more than three.
        assert_eq!(snap(&[0x80, 0x80, 0x80, 0x80, b'a'], 8, false), (3, 2));
        assert_eq!(snap(&[], 8, false), (0, 0));
    }
    #[test]
    fn j1t_10_open_end_at_a_watermark_leaves_an_incomplete_scalar_for_the_next_page() {
        let text = "aé€😀b".as_bytes();
        // Each scalar cut at the watermark is left whole for the next page.
        assert_eq!(snap(&text[..2], 8, true), (0, 1));
        assert_eq!(snap(&text[..4], 8, true), (0, 3));
        assert_eq!(snap(&text[..5], 8, true), (0, 3));
        assert_eq!(snap(&text[..7], 8, true), (0, 6));
        assert_eq!(snap(&text[..9], 8, true), (0, 6));
        // A page made only of an incomplete scalar stays empty.
        assert_eq!(snap(&text[6..8], 8, true), (0, 0));
        // Complete scalars and invalid bytes at the end are unchanged.
        assert_eq!(snap(&text[..10], 16, true), (0, 10));
        assert_eq!(snap(b"x\xff", 8, true), (0, 2));
        assert_eq!(snap(&[b'x', 0x80, 0x80, 0x80], 8, true), (0, 4));
        // A closed end keeps the incomplete bytes, to render as U+FFFD.
        assert_eq!(snap(&text[..9], 16, false), (0, 9));
        // The limit trim still applies before the open end.
        assert_eq!(snap(&text[..9], 2, true), (0, 1));
    }
    /// J3T-08 (unit): the delivered text never exceeds 8,192 bytes; the
    /// consumed length is what the text covers, and the text equals the
    /// lossy decoding of exactly those bytes.
    #[test]
    fn j3t_08_delivered_text_is_bounded_and_matches_the_consumed_bytes() {
        // Valid text under the bound is unchanged.
        assert_eq!(
            bounded_text("aé€😀".as_bytes(), TEXT_LIMIT),
            ("aé€😀".into(), 10)
        );
        // 8,192 invalid bytes would be 24,576 bytes of U+FFFD.
        let invalid = vec![0xFF; PAGE_LIMIT];
        let (text, consumed) = bounded_text(&invalid, TEXT_LIMIT);
        assert_eq!((text.len(), consumed), (8190, 2730));
        assert_eq!(text, String::from_utf8_lossy(&invalid[..consumed]));
        // A scalar that would cross the bound is left for the next page;
        // one that ends exactly at it is delivered.
        let mut mixed = vec![0xFF; 2730];
        mixed.extend("€".as_bytes());
        let (text, consumed) = bounded_text(&mixed, TEXT_LIMIT);
        assert_eq!((text.len(), consumed), (8190, 2730));
        let mut mixed = vec![0xFF; 2729];
        mixed.extend("ab€".as_bytes());
        let (text, consumed) = bounded_text(&mixed, TEXT_LIMIT);
        assert_eq!((text.len(), consumed), (8192, 2734));
        assert!(text.ends_with('€'));
        // A truncated multi-byte sequence is one replacement, consumed whole.
        let (text, consumed) = bounded_text(b"x\xE2\x82y", TEXT_LIMIT);
        assert_eq!((text.as_str(), consumed), ("x\u{FFFD}y", 4));
        // The first scalar is always delivered, even past a tiny bound.
        assert_eq!(bounded_text("😀b".as_bytes(), 1), ("😀".into(), 4));
        assert_eq!(bounded_text(&[], 1), (String::new(), 0));
        // A pseudo-random mix never exceeds the bound and reassembles exactly.
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut source = Vec::new();
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            source.push(match seed % 4 {
                0 => 0xFF,
                1 => 0xE2,
                _ => b'a' + (seed % 26) as u8,
            });
        }
        let mut offset = 0;
        while offset < source.len() {
            let end = (offset + PAGE_LIMIT).min(source.len());
            let (text, consumed) = bounded_text(&source[offset..end], TEXT_LIMIT);
            assert!(text.len() <= TEXT_LIMIT && consumed > 0);
            assert_eq!(
                text,
                String::from_utf8_lossy(&source[offset..offset + consumed])
            );
            offset += consumed;
        }
    }
    #[test]
    fn j1t_10_page_state_names_match_the_composer_interface() {
        let names: Vec<&str> = [
            PageState::Live,
            PageState::Complete,
            PageState::Incomplete,
            PageState::UnverifiedPrefix,
            PageState::Expired,
            PageState::Missing,
            PageState::Corrupt,
            PageState::Deleted,
            PageState::Busy,
            PageState::Denied,
            PageState::StaleCursor,
        ]
        .iter()
        .map(|s| s.as_str())
        .collect();
        assert_eq!(
            names,
            [
                "live",
                "complete",
                "incomplete",
                "unverified_prefix",
                "expired",
                "missing",
                "corrupt",
                "deleted",
                "busy",
                "denied",
                "stale_cursor"
            ]
        );
    }
}
