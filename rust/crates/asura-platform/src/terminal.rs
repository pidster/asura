//! File-status flags for the CLI's exclusive terminal session.
use std::{io, os::fd::RawFd};

/// Saves flags before enabling nonblocking input and output. The CLI must keep
/// stdin/stdout open and unchanged until restoration, and restore terminal modes
/// before this guard. This guard does not close or replace those descriptors.
pub struct TerminalIo {
    descriptors: [RawFd; 2],
    original: [libc::c_int; 2],
    pending: [bool; 2],
}
impl TerminalIo {
    pub fn acquire() -> io::Result<Self> {
        Self::acquire_with([libc::STDIN_FILENO, libc::STDOUT_FILENO], set_flags)
    }

    fn acquire_with(
        descriptors: [RawFd; 2],
        mut set: impl FnMut(RawFd, libc::c_int) -> io::Result<()>,
    ) -> io::Result<Self> {
        // Snapshot BOTH before any mutation: two descriptor numbers can refer
        // to the same open file description and therefore share status flags.
        let original = [get_flags(descriptors[0])?, get_flags(descriptors[1])?];
        let mut guard = Self {
            descriptors,
            original,
            pending: [false; 2],
        };
        for index in 0..2 {
            guard.pending[index] = true;
            if let Err(error) = set(descriptors[index], original[index] | libc::O_NONBLOCK) {
                return match guard.restore() {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(io::Error::new(
                        error.kind(),
                        format!(
                            "terminal flag setup failed: {error}; restoration failed: {cleanup}"
                        ),
                    )),
                };
            }
        }
        Ok(guard)
    }

    /// Attempt every pending restoration. A failed descriptor remains pending
    /// for an explicit retry or Drop; a successful one is not changed again.
    pub fn restore(&mut self) -> io::Result<()> {
        self.restore_with(set_flags)
    }

    fn restore_with(
        &mut self,
        mut set: impl FnMut(RawFd, libc::c_int) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut first_error = None;
        for index in (0..2).rev() {
            if self.pending[index] {
                match set(self.descriptors[index], self.original[index]) {
                    Ok(()) => self.pending[index] = false,
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                    }
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}
impl Drop for TerminalIo {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
fn get_flags(fd: RawFd) -> io::Result<libc::c_int> {
    // SAFETY: F_GETFL accepts an integer descriptor without any pointer. A bad
    // descriptor returns an OS error; ownership stays with the caller.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(flags)
    }
}
fn set_flags(fd: RawFd, flags: libc::c_int) -> io::Result<()> {
    // SAFETY: F_SETFL takes integer flags, not a pointer. The session retains
    // the descriptor and its exclusive flag ownership through restoration.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const OUTPUT_DEADLINE: std::time::Duration = std::time::Duration::from_millis(100);

/// Bounded frame output for a terminal with an active [`TerminalIo`] guard.
/// Keep that guard alive through every pump: stdout must remain nonblocking.
/// Drop does not write. Each pump yields after bounded progress or backpressure.
#[derive(Default)]
pub struct TerminalOutput {
    buffer: Vec<u8>,
    sent: usize,
    deadline: Option<std::time::Instant>,
}
impl io::Write for TerminalOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_FRAME_BYTES.saturating_sub(self.buffer.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal frame exceeds 4 MiB",
            ));
        }
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.pump()
    }
}
impl TerminalOutput {
    pub fn pending(&self) -> bool {
        self.sent < self.buffer.len()
    }

    /// Make at most 16 nonblocking writes and send at most 64 KiB. The event
    /// loop must call this again while pending, without drawing another frame.
    pub fn pump(&mut self) -> io::Result<()> {
        self.pump_with(std::time::Instant::now, write_stdout)
    }
    fn pump_with(
        &mut self,
        mut now: impl FnMut() -> std::time::Instant,
        mut write: impl FnMut(&[u8]) -> io::Result<usize>,
    ) -> io::Result<()> {
        if !self.pending() {
            return Ok(());
        }
        let deadline = *self.deadline.get_or_insert_with(|| now() + OUTPUT_DEADLINE);
        let mut budget = 64 * 1024;
        for _ in 0..16 {
            if now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "terminal output deadline",
                ));
            }
            let length = (self.buffer.len() - self.sent).min(budget);
            let remaining = &self.buffer[self.sent..self.sent + length];
            match write(remaining) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "terminal output made no progress",
                    ));
                }
                Ok(count) if count <= remaining.len() => {
                    self.sent += count;
                    budget -= count;
                }
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "terminal output returned invalid byte count",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
            if !self.pending() {
                self.buffer.clear();
                self.sent = 0;
                self.deadline = None;
                return Ok(());
            }
            if budget == 0 {
                break;
            }
        }
        Ok(())
    }
}
fn write_stdout(bytes: &[u8]) -> io::Result<usize> {
    // SAFETY: the slice remains readable for its exact length during the call.
    // TerminalIo retains nonblocking stdout; this function does not close it.
    let result = unsafe { libc::write(libc::STDOUT_FILENO, bytes.as_ptr().cast(), bytes.len()) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::{fd::AsRawFd, unix::net::UnixStream};

    #[test]
    fn isolated_flags_restore_exactly_and_drop_is_idempotent() {
        let (input, output) = UnixStream::pair().unwrap();
        output.set_nonblocking(true).unwrap();
        let fds = [input.as_raw_fd(), output.as_raw_fd()];
        let before = [get_flags(fds[0]).unwrap(), get_flags(fds[1]).unwrap()];
        let mut guard = TerminalIo::acquire_with(fds, set_flags).unwrap();
        for fd in fds {
            assert_ne!(get_flags(fd).unwrap() & libc::O_NONBLOCK, 0);
        }
        guard.restore().unwrap();
        guard.restore().unwrap();
        drop(guard);
        assert_eq!(
            [get_flags(fds[0]).unwrap(), get_flags(fds[1]).unwrap()],
            before
        );
    }
    #[test]
    fn shared_description_snapshots_precede_both_mutations() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let duplicate = stream.try_clone().unwrap();
        let fds = [stream.as_raw_fd(), duplicate.as_raw_fd()];
        let before = get_flags(fds[0]).unwrap();
        {
            let _guard = TerminalIo::acquire_with(fds, set_flags).unwrap();
            assert_ne!(get_flags(fds[1]).unwrap() & libc::O_NONBLOCK, 0);
        }
        assert_eq!(get_flags(fds[0]).unwrap(), before);
        assert_eq!(get_flags(fds[1]).unwrap(), before);
    }
    #[test]
    fn failed_second_snapshot_does_not_mutate_first() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let before = get_flags(stream.as_raw_fd()).unwrap();
        assert!(
            TerminalIo::acquire_with([stream.as_raw_fd(), -1], |_, _| panic!(
                "mutation before snapshots"
            ))
            .is_err()
        );
        assert_eq!(get_flags(stream.as_raw_fd()).unwrap(), before);
    }
    #[test]
    fn failed_second_mutation_rolls_back_first() {
        let (input, output) = UnixStream::pair().unwrap();
        let fds = [input.as_raw_fd(), output.as_raw_fd()];
        let before = [get_flags(fds[0]).unwrap(), get_flags(fds[1]).unwrap()];
        let result = TerminalIo::acquire_with(fds, |fd, flags| {
            if fd == fds[1] {
                Err(io::Error::from_raw_os_error(libc::EIO))
            } else {
                set_flags(fd, flags)
            }
        });
        assert_eq!(result.err().unwrap().raw_os_error(), Some(libc::EIO));
        assert_eq!(
            [get_flags(fds[0]).unwrap(), get_flags(fds[1]).unwrap()],
            before
        );
    }
    #[test]
    fn restoration_attempts_both_then_retries_only_failure() {
        let (input, output) = UnixStream::pair().unwrap();
        let fds = [input.as_raw_fd(), output.as_raw_fd()];
        let before = [get_flags(fds[0]).unwrap(), get_flags(fds[1]).unwrap()];
        let mut guard = TerminalIo::acquire_with(fds, set_flags).unwrap();
        let mut attempted = Vec::new();
        assert!(
            guard
                .restore_with(|fd, flags| {
                    attempted.push(fd);
                    if fd == fds[1] {
                        Err(io::Error::from_raw_os_error(libc::EIO))
                    } else {
                        set_flags(fd, flags)
                    }
                })
                .is_err()
        );
        assert_eq!(attempted, [fds[1], fds[0]]);
        attempted.clear();
        guard
            .restore_with(|fd, flags| {
                attempted.push(fd);
                set_flags(fd, flags)
            })
            .unwrap();
        assert_eq!(attempted, [fds[1]]);
        assert_eq!(
            [get_flags(fds[0]).unwrap(), get_flags(fds[1]).unwrap()],
            before
        );
    }
    #[test]
    fn output_partial_progress_backpressure_and_retry_preserve_exact_bytes() {
        use std::io::Write;
        let start = std::time::Instant::now();
        let mut output = TerminalOutput::default();
        output.write_all(b"abcdef").unwrap();
        let mut received = Vec::new();
        let mut calls = 0;
        output
            .pump_with(
                || start,
                |bytes| {
                    calls += 1;
                    match calls {
                        1 => {
                            received.extend_from_slice(&bytes[..2]);
                            Ok(2)
                        }
                        2 => Err(io::ErrorKind::Interrupted.into()),
                        _ => Err(io::ErrorKind::WouldBlock.into()),
                    }
                },
            )
            .unwrap();
        assert!(output.pending());
        assert_eq!(calls, 3);
        output
            .pump_with(
                || start + std::time::Duration::from_millis(10),
                |bytes| {
                    received.extend_from_slice(bytes);
                    Ok(bytes.len())
                },
            )
            .unwrap();
        assert_eq!(received, b"abcdef");
        assert!(!output.pending());
        assert!(output.buffer.is_empty());
        output
            .pump_with(
                || panic!("empty flush checks no clock"),
                |_| panic!("empty flush writes nothing"),
            )
            .unwrap();
    }
    #[test]
    fn output_deadline_is_absolute_across_progress_and_ticks() {
        use std::io::Write;
        let start = std::time::Instant::now();
        let mut output = TerminalOutput::default();
        output.write_all(b"abc").unwrap();
        output
            .pump_with(|| start, |_| Err(io::ErrorKind::WouldBlock.into()))
            .unwrap();
        let mut calls = 0;
        output
            .pump_with(
                || start + std::time::Duration::from_millis(99),
                |_| {
                    calls += 1;
                    if calls == 1 {
                        Ok(1)
                    } else {
                        Err(io::ErrorKind::WouldBlock.into())
                    }
                },
            )
            .unwrap();
        assert_eq!(output.sent, 1);
        assert_eq!(
            output
                .pump_with(
                    || start + OUTPUT_DEADLINE,
                    |_| panic!("expired output must not write")
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(output.pending());
    }
    #[test]
    fn output_capacity_and_per_pump_work_are_bounded() {
        use std::io::Write;
        let mut output = TerminalOutput::default();
        output.write_all(&vec![b'x'; MAX_FRAME_BYTES]).unwrap();
        assert!(output.write(b"x").is_err());
        assert_eq!(output.buffer.len(), MAX_FRAME_BYTES);
        let now = std::time::Instant::now();
        let mut bytes_sent = 0;
        output
            .pump_with(
                || now,
                |bytes| {
                    bytes_sent += bytes.len();
                    Ok(bytes.len())
                },
            )
            .unwrap();
        assert_eq!(bytes_sent, 65536);
        assert!(output.pending());
        let mut calls = 0;
        output
            .pump_with(
                || now,
                |_| {
                    calls += 1;
                    Err(io::ErrorKind::Interrupted.into())
                },
            )
            .unwrap();
        assert_eq!(calls, 16);
        assert_eq!(
            output.pump_with(|| now, |_| Ok(0)).unwrap_err().kind(),
            io::ErrorKind::WriteZero
        );
        assert_eq!(
            output
                .pump_with(|| now, |_| Err(io::ErrorKind::BrokenPipe.into()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }
}
