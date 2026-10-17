//! Reactor side of exec_cmd output capture (J1). Pipes are read without
//! blocking and raw bytes go straight to the service's [`OutputSink`]; no raw
//! ring or tail is kept here (G10).
use super::{ExecCmdError, spawn};
use std::os::fd::{AsRawFd, OwnedFd};

/// Which pipe a capture reads.
pub use crate::secrets::Stream;

/// The read buffer: a stream's read interest is omitted while the sink has
/// less room than this.
pub const READ_BUFFER: usize = 4096;
/// Bytes drained per stream per dispatch (L14).
pub const DRAIN_BUDGET: usize = 8192;

/// Receives raw output for one exec_cmd job on the reactor. Every method is
/// called on the reactor and must not block.
pub trait OutputSink: Send {
    /// Raw bytes the sink accepts now without blocking; 0 when full.
    fn room(&self) -> usize;
    /// Takes at most `room()` raw bytes read from one stream.
    fn accept(&mut self, stream: Stream, bytes: &[u8]);
    /// After a cancel trigger: `bytes` raw bytes of `stream` were read and
    /// dropped because the sink had no room (owner, 2026-10-15). From the
    /// first discarded byte every later byte of that stream is discarded too.
    fn discard(&mut self, stream: Stream, bytes: u64);
    /// The stream reached EOF.
    fn end(&mut self, stream: Stream);
    /// Called once per dispatch: move bytes that wait to form a chunk on.
    fn flush(&mut self) {}
}

pub(super) struct Capture {
    reader: OwnedFd,
    stream: Stream,
    pub eof: bool,
    pub total: u64,
    discarding: bool,
}
impl Capture {
    pub fn new(reader: OwnedFd, stream: Stream) -> Self {
        Self {
            reader,
            stream,
            eof: false,
            total: 0,
            discarding: false,
        }
    }
    pub fn fd(&self) -> i32 {
        self.reader.as_raw_fd()
    }
    /// Whether the reactor should poll this pipe: not at EOF, and either the
    /// sink has room for a full read or a cancel trigger requires draining.
    pub fn wants_read(&self, room: usize, stopping: bool) -> bool {
        !self.eof && (stopping || room >= READ_BUFFER)
    }
    /// Reads at most `min(budget, room)` bytes before a cancel trigger, so
    /// a full sink stops pipe reads and the producer blocks in the kernel.
    /// After a cancel trigger it reads up to `budget` and discards what the
    /// sink cannot take, so the killed group can reach EOF (J1-Q1).
    pub fn drain(
        &mut self,
        budget: usize,
        sink: &mut dyn OutputSink,
        stopping: bool,
    ) -> Result<(), ExecCmdError> {
        let mut used = 0;
        let mut buffer = [0u8; READ_BUFFER];
        while !self.eof && used < budget {
            let room = sink.room();
            if !stopping && room < READ_BUFFER {
                break;
            }
            let mut length = buffer.len().min(budget - used);
            if !stopping {
                length = length.min(room);
            }
            // SAFETY: owned nonblocking pipe, writable bounded buffer.
            let n = unsafe { libc::read(self.fd(), buffer.as_mut_ptr().cast(), length) };
            if n == 0 {
                self.eof = true;
                sink.end(self.stream);
                break;
            }
            if n < 0 {
                if spawn::would_block() {
                    break;
                }
                return Err(ExecCmdError::Io);
            }
            let n = n as usize;
            used += n;
            self.total = self.total.saturating_add(n as u64);
            let take = if self.discarding {
                0
            } else {
                n.min(sink.room())
            };
            if take > 0 {
                sink.accept(self.stream, &buffer[..take]);
            }
            if take < n {
                self.discarding = true;
                sink.discard(self.stream, (n - take) as u64);
            }
        }
        Ok(())
    }
}

/// The model-visible tail of redacted bytes: the last 6,144 bytes, invalid
/// UTF-8 and control characters other than newline and tab replaced with
/// U+FFFD. Returns the text and whether anything was cut or replaced. Runs
/// over redacted bytes only (J1, L13, L17 until J3).
pub fn render_tail(redacted: &[u8]) -> (String, bool) {
    let text = String::from_utf8_lossy(redacted);
    let mut lost = matches!(text, std::borrow::Cow::Owned(_));
    let chars: Vec<char> = text
        .chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                lost = true;
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect();
    // Keep the most recent output: a failure's cause is usually at the end.
    let mut start = chars.len();
    let mut size = 0;
    while start > 0 && size + chars[start - 1].len_utf8() <= 6144 {
        start -= 1;
        size += chars[start].len_utf8();
    }
    lost |= start > 0;
    (chars[start..].iter().collect(), lost)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tail_keeps_the_last_6144_bytes_and_replaces_controls() {
        let (text, cut) = render_tail(b"ok\n");
        assert_eq!((text.as_str(), cut), ("ok\n", false));
        let (text, cut) = render_tail(b"a\x07b\tc");
        assert_eq!((text.as_str(), cut), ("a\u{fffd}b\tc", true));
        let (text, cut) = render_tail(&[0xff, b'x']);
        assert_eq!((text.as_str(), cut), ("\u{fffd}x", true));
        let long: Vec<u8> = (0..8192).map(|i| b'a' + (i % 26) as u8).collect();
        let (text, cut) = render_tail(&long);
        assert!(cut);
        assert_eq!(text.len(), 6144);
        assert!(long.ends_with(text.as_bytes()));
        // A multi-byte scalar at the cut is never split.
        let mut wide = vec![b'x'; 6143];
        wide.extend_from_slice("é".as_bytes());
        let (text, cut) = render_tail(&wide);
        assert!(cut);
        assert!(text.starts_with('x') && text.ends_with('é'));
        assert!(text.len() <= 6144);
    }
}
