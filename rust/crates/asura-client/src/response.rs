//! Client-side assembly of one response and its thinking text from
//! observation windows (response-text-transfer.md, thinking-level.md). The
//! service keeps no per-client state; the client reports how many bytes of
//! each it holds.
use crate::{Error, Result};
use asura_control::pb;

/// One response as this client holds it.
#[derive(Clone, Debug, Default)]
pub struct ResponseAssembly {
    cursor: u64,
    text: String,
    total: u64,
    thinking: String,
    thinking_total: u64,
    terminal: bool,
    windows: bool,
}
impl ResponseAssembly {
    /// The cursor to observe after.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }
    /// Response bytes held, sent as `ConversationObserve.text_offset`.
    pub fn held(&self) -> u64 {
        self.text.len() as u64
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    /// Thinking bytes held, sent as `ConversationObserve.thinking_offset`.
    pub fn held_thinking(&self) -> u64 {
        self.thinking.len() as u64
    }
    pub fn thinking(&self) -> &str {
        &self.thinking
    }
    /// Applies one event: the thinking window, then the response window;
    /// each keeps its offset bytes and appends its text.
    pub fn apply(&mut self, event: &pb::ConversationEvent) -> Result<()> {
        if let Some(window) = &event.thinking_text {
            let offset = event.thinking_offset.ok_or(Error::Protocol)?;
            let total = event.thinking_bytes.ok_or(Error::Protocol)?;
            let start = usize::try_from(offset).map_err(|_| Error::Protocol)?;
            if start > self.thinking.len() || !self.thinking.is_char_boundary(start) {
                return Err(Error::Protocol);
            }
            self.thinking.truncate(start);
            self.thinking.push_str(window);
            if self.thinking.len() as u64 > total {
                return Err(Error::Protocol);
            }
            self.thinking_total = total;
            self.windows = true;
        }
        if let Some(window) = &event.text {
            let offset = event.text_offset.ok_or(Error::Protocol)?;
            let total = event.text_bytes.ok_or(Error::Protocol)?;
            let start = usize::try_from(offset).map_err(|_| Error::Protocol)?;
            if start > self.text.len() || !self.text.is_char_boundary(start) {
                return Err(Error::Protocol);
            }
            self.text.truncate(start);
            self.text.push_str(window);
            if self.text.len() as u64 > total {
                return Err(Error::Protocol);
            }
            self.total = total;
            self.windows = true;
        }
        if let Some(cursor) = event.cursor {
            self.cursor = cursor;
        }
        self.terminal = matches!(event.kind, Some(3..=6));
        Ok(())
    }
    /// The whole current response and thinking text are held.
    pub fn complete(&self) -> bool {
        self.text.len() as u64 == self.total && self.thinking.len() as u64 == self.thinking_total
    }
    /// The terminal response is held whole.
    pub fn done(&self) -> bool {
        self.terminal && self.complete()
    }
    /// `event` with its window replaced by the whole assembled response.
    pub fn whole(&self, mut event: pb::ConversationEvent) -> pb::ConversationEvent {
        if self.windows && event.text.is_some() {
            event.text = Some(self.text.clone());
            event.text_offset = Some(0);
            event.text_bytes = Some(self.total);
        }
        if self.windows && (event.thinking_text.is_some() || !self.thinking.is_empty()) {
            event.thinking_text = Some(self.thinking.clone());
            event.thinking_offset = Some(0);
            event.thinking_bytes = Some(self.thinking_total);
        }
        event
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(kind: i32, cursor: u64, offset: u64, text: &str, total: u64) -> pb::ConversationEvent {
        pb::ConversationEvent {
            kind: Some(kind),
            cursor: Some(cursor),
            text: Some(text.into()),
            text_offset: Some(offset),
            text_bytes: Some(total),
            ..Default::default()
        }
    }
    /// RT-03 and RT-04 (client): windows assemble, a cut truncates, and an
    /// offset beyond the held text or inside a character is a protocol error.
    #[test]
    fn rt03_windows_assemble_and_cuts_truncate() {
        let mut assembly = ResponseAssembly::default();
        assembly.apply(&event(2, 2, 0, "hello ", 11)).unwrap();
        assert!(!assembly.complete());
        assembly.apply(&event(2, 2, 6, "world", 11)).unwrap();
        assert!(assembly.complete() && !assembly.done());
        assembly.apply(&event(2, 3, 6, "there", 11)).unwrap();
        assert_eq!(assembly.text(), "hello there");
        assembly
            .apply(&event(3, u64::MAX, 0, "hello there!", 12))
            .unwrap();
        assert!(assembly.done());
        let whole = assembly.whole(event(3, u64::MAX, 12, "", 12));
        assert_eq!(whole.text.as_deref(), Some("hello there!"));
        assert_eq!(whole.text_offset, Some(0));
        assert!(assembly.apply(&event(2, 4, 13, "x", 20)).is_err());
        let mut multibyte = ResponseAssembly::default();
        multibyte.apply(&event(2, 2, 0, "é", 2)).unwrap();
        assert!(multibyte.apply(&event(2, 3, 1, "x", 2)).is_err());
        assert!(
            ResponseAssembly::default()
                .apply(&event(2, 2, 0, "abc", 2))
                .is_err()
        );
    }
    /// TL-09 and TL-12 (client): thinking windows assemble beside the
    /// response; the assembly is complete only when both are held whole; a
    /// cut truncates; an invalid offset is a protocol error.
    #[test]
    fn tl09_thinking_windows_assemble_beside_the_response() {
        let with = |event: pb::ConversationEvent, offset: u64, text: &str, total: u64| {
            pb::ConversationEvent {
                thinking_text: Some(text.into()),
                thinking_offset: Some(offset),
                thinking_bytes: Some(total),
                ..event
            }
        };
        let mut assembly = ResponseAssembly::default();
        assembly
            .apply(&with(event(2, 2, 0, "", 0), 0, "let me ", 13))
            .unwrap();
        assert!(!assembly.complete());
        assert_eq!(assembly.held_thinking(), 7);
        assembly
            .apply(&with(event(2, 3, 0, "Hi", 2), 7, "think", 13))
            .unwrap();
        assert!(!assembly.complete());
        assembly
            .apply(&with(event(2, 4, 2, "", 2), 4, "\u{e9} think!", 13))
            .unwrap();
        assert_eq!(assembly.thinking(), "let \u{e9} think!");
        assert!(assembly.complete());
        let whole = assembly.whole(event(3, u64::MAX, 2, "", 2));
        assert_eq!(whole.thinking_text.as_deref(), Some("let \u{e9} think!"));
        assert_eq!(whole.thinking_offset, Some(0));
        assert_eq!(whole.thinking_bytes, Some(13));
        // An offset inside a character.
        assert!(
            assembly
                .apply(&with(event(2, 5, 2, "", 2), 5, "x", 13))
                .is_err()
        );
        assert!(
            ResponseAssembly::default()
                .apply(&pb::ConversationEvent {
                    thinking_text: Some("x".into()),
                    ..event(2, 2, 0, "", 0)
                })
                .is_err()
        );
    }
}
