use super::{ShellError, spawn};
use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};
pub(super) struct Capture {
    reader: OwnedFd,
    bytes: VecDeque<u8>,
    pub eof: bool,
    pub total: u64,
    lost: bool,
}
impl Capture {
    pub fn new(reader: OwnedFd) -> Self {
        Self {
            reader,
            bytes: VecDeque::with_capacity(8192),
            eof: false,
            total: 0,
            lost: false,
        }
    }
    pub fn fd(&self) -> i32 {
        self.reader.as_raw_fd()
    }
    pub fn drain(&mut self, budget: usize) -> Result<(), ShellError> {
        let mut used = 0;
        let mut buffer = [0u8; 4096];
        while !self.eof && used < budget {
            let length = buffer.len().min(budget - used);
            // SAFETY: owned nonblocking pipe, writable bounded buffer.
            let n = unsafe { libc::read(self.fd(), buffer.as_mut_ptr().cast(), length) };
            if n == 0 {
                self.eof = true;
                break;
            }
            if n < 0 {
                if spawn::would_block() {
                    break;
                }
                return Err(ShellError::Io);
            }
            let n = n as usize;
            used += n;
            self.total = self.total.saturating_add(n as u64);
            for b in &buffer[..n] {
                if self.bytes.len() == 8192 {
                    self.bytes.pop_front();
                    self.lost = true;
                }
                self.bytes.push_back(*b);
            }
        }
        Ok(())
    }
    pub fn text(&self) -> (String, bool) {
        let bytes: Vec<_> = self.bytes.iter().copied().collect();
        let text = String::from_utf8_lossy(&bytes);
        let mut lost = self.lost || matches!(text, std::borrow::Cow::Owned(_));
        let chars: Vec<char> = text
            .chars()
            .map(|c| {
                if c.is_control() && c != '\n' && c != '\t' {
                    lost = true;
                    '�'
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
}
