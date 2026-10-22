//! The active response (response-text-transfer.md): its text, the prefix the
//! journal holds, recent cuts for observation windows and the output pause.
//! The active thinking text (thinking-level.md) uses the same state; it has
//! no kind 8 tail, so the terminal flush writes all of it as kind 25.
//! Pure state; the conversation owner submits journal commands and credit.
use asura_control::MAX_RESPONSE_WINDOW_BYTES;
use asura_storage::authority::conversation::{MAX_TEXT, MAX_WINDOW_TEXT};
use std::collections::VecDeque;

// A terminal window read from the journal travels in the same event as a
// live window, so the storage bound must equal the control bound
// (response-text-transfer.md, event window bound).
const _: () = assert!(MAX_WINDOW_TEXT == MAX_RESPONSE_WINDOW_BYTES);

/// Text in one kind 24 segment or the kind 8 tail.
pub(super) const SEGMENT_BYTES: usize = MAX_TEXT;
/// Unjournaled bytes at which the helper's next revision credit is withheld.
pub(super) const PAUSE_BYTES: usize = 2 * SEGMENT_BYTES;
/// Recent cuts kept for observation windows.
const CUTS: usize = 16;

#[derive(Debug, Default)]
pub(super) struct Response {
    text: String,
    /// The prefix length that the journal holds byte for byte.
    stable: usize,
    /// A kind 24 segment in flight: its start and end in `text`.
    pending: Option<(usize, usize)>,
    /// The lowest offset revised while the segment was in flight.
    pending_floor: usize,
    /// The journal refused a segment for capacity; no more segments.
    exhausted: bool,
    /// Recent cuts: the cursor after the cut and the kept length.
    cuts: VecDeque<(u64, usize)>,
    /// The newest cursor of a cut no longer in `cuts`.
    forgotten: u64,
}

fn floor(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

impl Response {
    pub(super) fn len(&self) -> usize {
        self.text.len()
    }
    pub(super) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
    fn cut(&mut self, cursor: u64, offset: usize) {
        if self.cuts.len() == CUTS
            && let Some((dropped, _)) = self.cuts.pop_front()
        {
            self.forgotten = dropped;
        }
        self.cuts.push_back((cursor, offset));
    }
    /// Applies one validated revision. `cursor` is the cursor after it.
    pub(super) fn revise(
        &mut self,
        offset: u64,
        appended: &str,
        cursor: u64,
    ) -> Result<(), &'static str> {
        let offset = usize::try_from(offset).map_err(|_| "model_protocol_fault")?;
        if offset > self.text.len() || !self.text.is_char_boundary(offset) {
            return Err("model_protocol_fault");
        }
        if offset < self.text.len() {
            self.cut(cursor, offset);
        }
        self.text.truncate(offset);
        self.text.push_str(appended);
        self.stable = self.stable.min(offset);
        self.pending_floor = self.pending_floor.min(offset);
        Ok(())
    }
    /// Bytes the journal does not hold yet.
    pub(super) fn backlog(&self) -> usize {
        self.text.len() - self.stable
    }
    /// Withhold the helper's next revision credit.
    pub(super) fn paused(&self) -> bool {
        !self.exhausted && self.backlog() >= PAUSE_BYTES
    }
    /// The next kind 24 segment, while the tail is too large for the kind 8
    /// frame. Returns its offset and text.
    pub(super) fn next_segment(&mut self) -> Option<(u64, String)> {
        if self.exhausted || self.pending.is_some() || self.backlog() <= SEGMENT_BYTES {
            return None;
        }
        let start = self.stable;
        let end = floor(&self.text, start + SEGMENT_BYTES);
        self.pending = Some((start, end));
        self.pending_floor = usize::MAX;
        Some((start as u64, self.text[start..end].to_owned()))
    }
    /// The next kind 25 segment at the terminal flush: any unjournaled
    /// bytes, at most one segment.
    pub(super) fn flush_segment(&mut self) -> Option<(u64, String)> {
        if self.exhausted || self.pending.is_some() || self.backlog() == 0 {
            return None;
        }
        let start = self.stable;
        let end = floor(&self.text, start + SEGMENT_BYTES);
        self.pending = Some((start, end));
        self.pending_floor = usize::MAX;
        Some((start as u64, self.text[start..end].to_owned()))
    }
    /// Nothing remains to journal.
    pub(super) fn flushed(&self) -> bool {
        self.pending.is_none() && (self.exhausted || self.backlog() == 0)
    }
    /// The journal refused a thinking segment for capacity: keep only what
    /// it holds, since no kind 8 tail carries thinking.
    pub(super) fn exhaust_to_journaled(&mut self, cursor: u64) {
        self.pending = None;
        self.exhausted = true;
        if self.stable < self.text.len() {
            self.cut(cursor, self.stable);
            self.text.truncate(self.stable);
        }
    }
    /// The pending segment committed.
    pub(super) fn committed(&mut self) {
        if let Some((_, end)) = self.pending.take() {
            self.stable = end.min(self.pending_floor).min(self.text.len());
        }
    }
    /// The journal held the pending segment at its quota: nothing was
    /// written; the same bytes are offered again when the hold ends.
    pub(super) fn is_exhausted(&self) -> bool {
        self.exhausted
    }
    pub(super) fn refused(&mut self) {
        self.pending = None;
    }
    /// The journal refused the pending segment for capacity: keep what it
    /// holds plus one tail, and tell observers through a cut.
    pub(super) fn exhaust(&mut self, cursor: u64) {
        self.pending = None;
        self.exhausted = true;
        let end = floor(&self.text, self.stable + SEGMENT_BYTES);
        if end < self.text.len() {
            self.cut(cursor, end);
            self.text.truncate(end);
        }
    }
    /// The kind 8 tail: the journaled prefix length and the remaining text.
    /// `None` while a segment must commit first.
    pub(super) fn tail(&self) -> Option<(u64, String)> {
        (self.pending.is_none() && self.backlog() <= SEGMENT_BYTES)
            .then(|| (self.stable as u64, self.text[self.stable..].to_owned()))
    }
    /// The window for a client at cursor `after` holding `held` bytes: the
    /// start (the held count, or the lowest cut since `after`) and at most
    /// one window of text.
    pub(super) fn window(&self, after: u64, held: u64) -> (u64, &str) {
        self.window_within(after, held, MAX_RESPONSE_WINDOW_BYTES)
    }
    /// [`Self::window`] with at most `budget` bytes of text: thinking and
    /// response windows share one event budget (thinking-level.md).
    pub(super) fn window_within(&self, after: u64, held: u64, budget: usize) -> (u64, &str) {
        let mut start = usize::try_from(held).unwrap_or(usize::MAX);
        if after < self.forgotten {
            start = 0;
        }
        for &(cursor, offset) in &self.cuts {
            if cursor > after {
                start = start.min(offset);
            }
        }
        let start = floor(&self.text, start);
        let end = floor(&self.text, start + budget.min(MAX_RESPONSE_WINDOW_BYTES));
        (start as u64, &self.text[start..end])
    }
    /// The client at `after` holding `held` bytes has the whole response.
    pub(super) fn caught_up(&self, after: u64, held: u64) -> bool {
        let (start, text) = self.window(after, held);
        text.is_empty() && start == held
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(client: &mut String, start: u64, text: &str) {
        client.truncate(start as usize);
        client.push_str(text);
    }

    /// RT-03 and RT-06: windows of at most 61,440 bytes rebuild a long
    /// response; the service keeps no per-client state, so a late client
    /// costs nothing and still assembles it whole.
    #[test]
    fn rt03_windows_rebuild_a_long_response_for_a_late_client() {
        let mut response = Response::default();
        let full = "a😀é".repeat(30_000);
        let mut cursor = 1;
        let mut sent = 0;
        while sent < full.len() {
            let end = floor(&full, sent + 61_440);
            cursor += 1;
            response
                .revise(sent as u64, &full[sent..end], cursor)
                .unwrap();
            sent = end;
        }
        let mut client = String::new();
        let mut rounds = 0;
        while !response.caught_up(0, client.len() as u64) {
            let (start, text) = response.window(0, client.len() as u64);
            assert!(text.len() <= MAX_RESPONSE_WINDOW_BYTES);
            apply(&mut client, start, text);
            rounds += 1;
        }
        assert_eq!(client, full);
        assert!(rounds >= full.len() / MAX_RESPONSE_WINDOW_BYTES);
    }

    /// RT-04: a cut below the held count restarts the window at the cut; a
    /// cursor older than a forgotten cut restarts at 0.
    #[test]
    fn rt04_cuts_resynchronize_clients() {
        let mut response = Response::default();
        response.revise(0, "hello world", 2).unwrap();
        let mut client = String::new();
        let (start, text) = response.window(0, 0);
        apply(&mut client, start, text);
        response.revise(6, "there", 3).unwrap();
        let (start, text) = response.window(2, client.len() as u64);
        assert_eq!((start, text), (6, "there"));
        apply(&mut client, start, text);
        assert_eq!(client, "hello there");
        assert!(response.caught_up(3, 11));
        // Seventeen more cuts forget the first ones.
        for cursor in 4..21 {
            response.revise(5, "!", cursor).unwrap();
        }
        assert_eq!(response.window(3, 11).0, 0);
        assert_eq!(response.window(19, 6).0, 5);
        assert_eq!(response.window(20, 6), (6, ""));
        // An offset inside a character or beyond the text is a fault.
        response.revise(0, "é", 21).unwrap();
        assert!(response.revise(1, "x", 22).is_err());
        assert!(response.revise(3, "x", 22).is_err());
    }

    /// RT-05: segments start at the journaled prefix, a revision during a
    /// pending segment lowers the stable prefix, and output pauses at 122,880
    /// unjournaled bytes.
    #[test]
    fn rt05_segments_and_pause_bound_the_unjournaled_tail() {
        let mut response = Response::default();
        response.revise(0, &"x".repeat(SEGMENT_BYTES), 2).unwrap();
        assert!(response.next_segment().is_none());
        assert_eq!(response.tail().unwrap().0, 0);
        response
            .revise(SEGMENT_BYTES as u64, &"y".repeat(SEGMENT_BYTES), 3)
            .unwrap();
        assert!(response.paused());
        assert!(response.tail().is_none());
        let (offset, text) = response.next_segment().unwrap();
        assert_eq!((offset, text.len()), (0, SEGMENT_BYTES));
        assert!(response.next_segment().is_none());
        // A rewrite while the segment is in flight.
        response.revise(100, "z", 4).unwrap();
        response.committed();
        assert_eq!(response.backlog(), 1);
        assert!(!response.paused());
        assert_eq!(response.tail().unwrap(), (100, "z".into()));
    }

    /// TL-12: thinking flushes every unjournaled byte at the terminal in
    /// segments of at most 61,440 bytes; a capacity refusal keeps only the
    /// journaled prefix; windows honor a smaller shared budget on character
    /// boundaries.
    #[test]
    fn tl12_thinking_flush_exhaust_and_budgeted_windows() {
        let mut thinking = Response::default();
        thinking
            .revise(0, &"é".repeat(SEGMENT_BYTES / 2 + 10), 2)
            .unwrap();
        let (offset, text) = thinking.flush_segment().unwrap();
        assert_eq!((offset, text.len()), (0, SEGMENT_BYTES));
        assert!(thinking.flush_segment().is_none() && !thinking.flushed());
        thinking.committed();
        // A small backlog waits for the terminal flush.
        assert!(thinking.next_segment().is_none());
        let (offset, text) = thinking.flush_segment().unwrap();
        assert_eq!((offset, text.len()), (SEGMENT_BYTES as u64, 20));
        thinking.committed();
        assert!(thinking.flushed() && thinking.flush_segment().is_none());
        // A budget cuts on a character boundary.
        assert_eq!(thinking.window_within(0, 0, 5).1, "éé");
        assert_eq!(thinking.window_within(0, 0, 0), (0, ""));
        // Exhaustion keeps the journaled prefix and tells observers.
        let mut thinking = Response::default();
        thinking.revise(0, "abcdef", 2).unwrap();
        let _ = thinking.flush_segment();
        thinking.committed();
        thinking.revise(6, "ghi", 3).unwrap();
        assert!(thinking.flush_segment().is_some());
        thinking.exhaust_to_journaled(4);
        assert_eq!(thinking.len(), 6);
        assert!(thinking.flushed() && thinking.flush_segment().is_none());
        assert_eq!(thinking.window(3, 9), (6, ""));
    }

    /// RT-07: a refused segment keeps the journaled prefix plus one tail and
    /// records a cut; no further segment is attempted.
    #[test]
    fn rt07_capacity_refusal_truncates_to_one_tail() {
        let mut response = Response::default();
        response
            .revise(0, &"x".repeat(3 * SEGMENT_BYTES), 2)
            .unwrap();
        assert!(response.next_segment().is_some());
        response.exhaust(3);
        assert!(!response.paused());
        assert!(response.next_segment().is_none());
        assert_eq!(response.len(), SEGMENT_BYTES);
        assert_eq!(response.tail().unwrap().0, 0);
        assert_eq!(
            response.window(2, 3 * SEGMENT_BYTES as u64),
            (SEGMENT_BYTES as u64, "")
        );
    }
}
