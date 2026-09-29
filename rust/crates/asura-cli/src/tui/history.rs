//! Bounded session-local recall. Recalling text never submits it.
use super::editor::MAX_DRAFT_BYTES;
use std::collections::VecDeque;

const MAX_ENTRIES: usize = 100;
const MAX_BYTES: usize = 1_048_576;

#[derive(Default)]
pub(super) struct History {
    entries: VecDeque<String>,
    bytes: usize,
    selected: Option<usize>,
    draft: Option<String>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, text: &str) {
        self.reset_navigation();
        if text.trim().is_empty()
            || text.len() > MAX_DRAFT_BYTES
            || self.entries.back().is_some_and(|last| last == text)
        {
            return;
        }
        self.bytes += text.len();
        self.entries.push_back(text.into());
        while self.entries.len() > MAX_ENTRIES || self.bytes > MAX_BYTES {
            if let Some(oldest) = self.entries.pop_front() {
                self.bytes -= oldest.len();
            }
        }
    }

    pub fn previous(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() || current.len() > MAX_DRAFT_BYTES {
            return None;
        }
        let selected = match self.selected {
            Some(0) => return None,
            Some(index) => index - 1,
            None => {
                self.draft = Some(current.into());
                self.entries.len() - 1
            }
        };
        self.selected = Some(selected);
        self.entries.get(selected).cloned()
    }

    pub fn next(&mut self) -> Option<String> {
        let selected = self.selected? + 1;
        if selected < self.entries.len() {
            self.selected = Some(selected);
            self.entries.get(selected).cloned()
        } else {
            self.selected = None;
            self.draft.take()
        }
    }

    pub fn reset_navigation(&mut self) {
        self.selected = None;
        self.draft = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_restores_exact_draft_and_stops_at_ends() {
        let mut history = History::new();
        assert_eq!(history.previous("draft"), None);
        history.record("/help");
        history.record("hello\n界");
        assert_eq!(history.previous("unsent\n👩‍💻"), Some("hello\n界".into()));
        assert_eq!(history.previous("hello\n界"), Some("/help".into()));
        assert_eq!(history.previous("/help"), None);
        assert_eq!(history.next(), Some("hello\n界".into()));
        assert_eq!(history.next(), Some("unsent\n👩‍💻".into()));
        assert_eq!(history.next(), None);
    }

    #[test]
    fn editing_recall_starts_a_new_draft_and_deduplicates_only_neighbors() {
        let mut history = History::new();
        history.record("one");
        history.record("one");
        history.record("  \n");
        history.record("two");
        history.record("one");
        assert_eq!(history.entries.len(), 3);
        history.previous("draft");
        history.reset_navigation();
        assert_eq!(history.previous("edited"), Some("one".into()));
        assert_eq!(history.next(), Some("edited".into()));
    }

    #[test]
    fn count_and_utf8_byte_limits_evict_oldest_entries() {
        let mut history = History::new();
        for n in 0..101 {
            history.record(&n.to_string());
        }
        assert_eq!(history.entries.len(), MAX_ENTRIES);
        assert_eq!(history.entries.front().unwrap(), "1");
        for n in 0..30 {
            history.record(&format!("{n:02}{}", "界".repeat(21_844)));
        }
        assert!(history.bytes <= MAX_BYTES);
        assert_eq!(
            history.bytes,
            history.entries.iter().map(String::len).sum::<usize>()
        );
        assert!(history.entries.len() < MAX_ENTRIES);
        let entries = history.entries.clone();
        history.record(&"x".repeat(MAX_DRAFT_BYTES + 1));
        assert_eq!(history.entries, entries);
        assert_eq!(history.previous(&"x".repeat(MAX_DRAFT_BYTES + 1)), None);
    }
}
