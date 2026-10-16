//! Text pages over retained payloads and the page-read pool (J1). J1 has no
//! page consumer; J3 and J4 add the model and client callers.
use super::store::{Location, PayloadId, PayloadState, PayloadStore};
use crate::authority::conversation::{Hash, Id};
use asura_platform::payload_io::PayloadError;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Source bytes per page.
pub const PAGE_LIMIT: usize = 8192;
/// Queued page requests; the next one is `busy`.
pub const PAGE_QUEUE: usize = 8;
/// The page read storage step (owner-approved wind-down bound): a read not
/// returned within it answers `busy` and the read continues on its worker.
pub const PAGE_STEP: Duration = Duration::from_secs(2);

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

/// Blocking: reads one text page. Page-read worker only.
pub fn read_page(store: &PayloadStore, request: PageRequest) -> Page {
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
    let bytes = &buffer[skip..skip + length];
    page.start = request.offset + skip as u64;
    page.text = String::from_utf8_lossy(bytes).into_owned();
    page.next_offset = page.start + length as u64;
    page
}

/// Who asked for a page. One worker is reserved for model requests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Origin {
    Model,
    Client,
}

#[derive(Default)]
struct Slot {
    page: Mutex<Option<Page>>,
    ready: Condvar,
    abandoned: std::sync::atomic::AtomicBool,
}
struct Job {
    request: PageRequest,
    origin: Origin,
    slot: Arc<Slot>,
}
#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
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

/// A submitted page read.
pub struct Ticket {
    slot: Arc<Slot>,
    submitted: Instant,
}
impl Ticket {
    /// Nonblocking: the page, or `busy` once the 2 s step has passed without
    /// one. A `busy` answer abandons the request; a running read continues
    /// on its worker, which stays held until the call returns.
    pub fn poll(&self, now: Instant) -> Option<Page> {
        if let Some(page) = self
            .slot
            .page
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            return Some(page);
        }
        if now >= self.submitted + PAGE_STEP {
            self.slot.abandoned.store(true, Ordering::Release);
            return Some(Page::empty(PageState::Busy, 0));
        }
        None
    }
    /// Worker-side wait for the page, bounded by the 2 s page step.
    pub fn wait(&self) -> Page {
        let end = self.submitted + PAGE_STEP;
        let mut page = self.slot.page.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(page) = page.take() {
                return page;
            }
            let now = Instant::now();
            if now >= end {
                self.slot.abandoned.store(true, Ordering::Release);
                return Page::empty(PageState::Busy, 0);
            }
            page = self
                .slot
                .ready
                .wait_timeout(page, end - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// Two retained page-read workers, one reserved for model requests, and a
/// queue of 8 (J1 numeric limits). No replacement worker ever starts.
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
        for (index, model_only) in [(0, false), (1, true)] {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name(format!("asura-page-read-{index}"))
                .spawn(move || worker(&shared, model_only))?;
        }
        Ok(Self { shared })
    }
    /// Queues a request; the ninth queued request is `busy`.
    pub fn submit(&self, request: PageRequest, origin: Origin) -> Result<Ticket, PageState> {
        let mut queue = lock(&self.shared);
        if queue.closed || queue.jobs.len() >= PAGE_QUEUE {
            return Err(PageState::Busy);
        }
        let slot = Arc::new(Slot::default());
        queue.jobs.push_back(Job {
            request,
            origin,
            slot: slot.clone(),
        });
        drop(queue);
        self.shared.work.notify_all();
        Ok(Ticket {
            slot,
            submitted: Instant::now(),
        })
    }
    /// Queued requests not yet taken by a worker.
    pub fn queued(&self) -> usize {
        lock(&self.shared).jobs.len()
    }
    /// Test hook: workers hold each taken request until released.
    #[cfg(any(test, feature = "test-support"))]
    pub fn hold_for_test(&self, held: bool) {
        lock(&self.shared).held_for_test = held;
        self.shared.work.notify_all();
    }
    /// Stops new requests; workers exit after their current read.
    pub fn close(&self) {
        lock(&self.shared).closed = true;
        self.shared.work.notify_all();
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
                    break queue.jobs.remove(position).expect("queued job");
                }
                queue = shared.work.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
        };
        // Test hold: the read stays in progress on this worker.
        loop {
            let queue = lock(shared);
            if !queue.held_for_test || queue.closed {
                break;
            }
            drop(
                shared
                    .work
                    .wait_timeout(queue, Duration::from_millis(5))
                    .unwrap_or_else(|e| e.into_inner()),
            );
        }
        if job.slot.abandoned.load(Ordering::Acquire) {
            continue;
        }
        let page = read_page(&shared.store, job.request);
        *job.slot.page.lock().unwrap_or_else(|e| e.into_inner()) = Some(page);
        job.slot.ready.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
                "busy",
                "denied",
                "stale_cursor"
            ]
        );
    }
}
