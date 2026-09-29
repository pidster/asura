//! One owner for terminal modes, rollback and process interruption flags.

use std::io::{self, IsTerminal, Write};
use std::os::{
    fd::{AsRawFd, RawFd},
    unix::net::UnixStream,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once, Weak};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use signal_hook::SigId;
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};

static ACTIVE_MODES: Mutex<Weak<Modes>> = Mutex::new(Weak::new());
static PANIC_HOOK: Once = Once::new();

/// Acquired modes and signal handlers for the single interactive session.
///
/// Call [`Self::restore`] before reporting errors. Drop retries failed restoration
/// as a fallback, but cannot report whether the terminal accepted that retry.
pub struct Session {
    modes: Arc<Modes>,
    interruption: Arc<AtomicBool>,
    signals: Vec<SigId>,
    signal_reader: UnixStream,
}

impl Session {
    /// Check both streams, register signals, and acquire the terminal modes.
    pub fn enter() -> io::Result<Self> {
        require_terminal(io::stdin().is_terminal(), io::stdout().is_terminal())?;

        let modes = Arc::new(Modes::default());
        {
            let mut active = ACTIVE_MODES.lock().unwrap_or_else(|e| e.into_inner());
            if active.upgrade().is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "an interactive terminal session is already active",
                ));
            }
            *active = Arc::downgrade(&modes);
        }

        let (signal_reader, signal_writer) = UnixStream::pair()?;
        signal_reader.set_nonblocking(true)?;
        signal_writer.set_nonblocking(true)?;
        let mut session = Self {
            modes,
            interruption: Arc::new(AtomicBool::new(false)),
            signals: Vec::with_capacity(6),
            signal_reader,
        };
        for signal in [SIGINT, SIGTERM, SIGHUP] {
            // A partially registered set is unregistered by Session's Drop if a
            // later registration fails, before any terminal modes are changed.
            session.signals.push(signal_hook::low_level::pipe::register(
                signal,
                signal_writer.try_clone()?,
            )?);
            session.signals.push(signal_hook::flag::register(
                signal,
                Arc::clone(&session.interruption),
            )?);
        }

        let terminal_io = asura_platform::TerminalIo::acquire()?;
        session.modes.manages_io.store(true, Ordering::Relaxed);
        *session
            .modes
            .terminal_io
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(terminal_io);
        if let Err(setup_error) = session.modes.acquire(&mut HostTerminal) {
            return Err(match session.restore() {
                Ok(()) => setup_error,
                Err(cleanup_error) => io::Error::new(
                    setup_error.kind(),
                    format!("{setup_error}; terminal rollback also failed: {cleanup_error}"),
                ),
            });
        }
        Ok(session)
    }

    /// Attempt every outstanding cleanup step, even when an earlier step fails.
    ///
    /// Successful steps are not repeated. Failed steps retain their ownership so
    /// another call or Drop can retry them. The first error kind is preserved and
    /// every failed step is named in the error message.
    pub fn restore(&mut self) -> io::Result<()> {
        self.modes.restore(&mut HostTerminal)
    }

    /// Settle transient terminal backpressure after interactive processing ends.
    pub fn finish(&mut self) -> io::Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        loop {
            match self.restore() {
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                result => return result,
            }
        }
    }

    /// Whether SIGINT, SIGTERM or SIGHUP requested loop termination.
    pub fn signal_fd(&self) -> RawFd {
        self.signal_reader.as_raw_fd()
    }
    pub fn interrupted(&self) -> bool {
        self.interruption.load(Ordering::Relaxed)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.restore();
        for registration in self.signals.drain(..) {
            signal_hook::low_level::unregister(registration);
        }
        let mut active = ACTIVE_MODES.lock().unwrap_or_else(|e| e.into_inner());
        if active.ptr_eq(&Arc::downgrade(&self.modes)) {
            *active = Weak::new();
        }
    }
}

/// Install once, before entering the terminal, preserving the previous hook.
///
/// Restoration runs before panic diagnostics. A restoration error is reported
/// explicitly; a successful write alone cannot establish physical terminal state.
pub fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let modes = ACTIVE_MODES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .upgrade();
            if let Some(modes) = modes
                && let Err(error) = modes.restore(&mut HostTerminal)
            {
                let _ = writeln!(
                    io::stderr(),
                    "terminal restoration during panic failed: {error}"
                );
            }
            previous(info);
        }));
    });
}

fn require_terminal(stdin: bool, stdout: bool) -> io::Result<()> {
    if stdin && stdout {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "interactive mode requires terminal stdin and stdout",
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Raw,
    Alternate,
    HiddenCursor,
    Paste,
}

impl Mode {
    const ORDER: [Self; 4] = [Self::Raw, Self::Alternate, Self::HiddenCursor, Self::Paste];

    fn name(self) -> &'static str {
        match self {
            Self::Raw => "raw mode",
            Self::Alternate => "alternate screen",
            Self::HiddenCursor => "hidden cursor",
            Self::Paste => "bracketed paste",
        }
    }
}

trait ModeControl {
    fn set(&mut self, mode: Mode, enabled: bool) -> io::Result<()>;
}

struct HostTerminal;

impl ModeControl for HostTerminal {
    fn set(&mut self, mode: Mode, enabled: bool) -> io::Result<()> {
        match (mode, enabled) {
            (Mode::Raw, true) => enable_raw_mode(),
            (Mode::Raw, false) => disable_raw_mode(),
            (Mode::Alternate, true) => execute!(io::stdout(), EnterAlternateScreen),
            (Mode::Alternate, false) => execute!(io::stdout(), LeaveAlternateScreen),
            (Mode::HiddenCursor, true) => execute!(io::stdout(), Hide),
            (Mode::HiddenCursor, false) => execute!(io::stdout(), Show),
            (Mode::Paste, true) => execute!(io::stdout(), EnableBracketedPaste),
            (Mode::Paste, false) => execute!(io::stdout(), DisableBracketedPaste),
        }
    }
}

#[derive(Default)]
struct Modes {
    manages_io: AtomicBool,
    terminal_io: Mutex<Option<asura_platform::TerminalIo>>,
    raw: AtomicBool,
    alternate: AtomicBool,
    hidden_cursor: AtomicBool,
    paste: AtomicBool,
}

impl Modes {
    fn owned(&self, mode: Mode) -> &AtomicBool {
        match mode {
            Mode::Raw => &self.raw,
            Mode::Alternate => &self.alternate,
            Mode::HiddenCursor => &self.hidden_cursor,
            Mode::Paste => &self.paste,
        }
    }

    fn acquire(&self, terminal: &mut impl ModeControl) -> io::Result<()> {
        for mode in Mode::ORDER {
            // A write or flush may fail after the terminal has accepted a mode.
            // Treat an attempted acquisition as owned until cleanup succeeds.
            self.owned(mode).store(true, Ordering::Relaxed);
            terminal.set(mode, true).map_err(|error| {
                io::Error::new(error.kind(), format!("enable {}: {error}", mode.name()))
            })?;
        }
        Ok(())
    }

    fn restore(&self, terminal: &mut impl ModeControl) -> io::Result<()> {
        // A previous partial cleanup may already have restored descriptor flags.
        // Keep retries nonblocking too, before retrying any terminal write.
        if self.manages_io.load(Ordering::Relaxed)
            && Mode::ORDER
                .iter()
                .any(|mode| self.owned(*mode).load(Ordering::Relaxed))
        {
            let mut guard = self.terminal_io.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_none() {
                *guard = Some(asura_platform::TerminalIo::acquire()?);
            }
        }
        let mut failures = Vec::new();
        let mut first_kind = None;
        for mode in Mode::ORDER.into_iter().rev() {
            if self.owned(mode).load(Ordering::Relaxed) {
                match terminal.set(mode, false) {
                    Ok(()) => self.owned(mode).store(false, Ordering::Relaxed),
                    Err(error) => {
                        first_kind.get_or_insert(error.kind());
                        failures.push(format!("restore {}: {error}", mode.name()));
                    }
                }
            }
        }
        let mut terminal_io = self.terminal_io.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(guard) = terminal_io.as_mut() {
            match guard.restore() {
                Ok(()) => *terminal_io = None,
                Err(error) => {
                    first_kind.get_or_insert(error.kind());
                    failures.push(format!("restore descriptor flags: {error}"));
                }
            }
        }
        match first_kind {
            Some(kind) => Err(io::Error::new(kind, failures.join("; "))),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Mode, ModeControl, Modes, require_terminal};
    use std::io;

    #[derive(Default)]
    struct FakeTerminal {
        operations: Vec<(Mode, bool)>,
        failures: Vec<(Mode, bool)>,
    }

    impl ModeControl for FakeTerminal {
        fn set(&mut self, mode: Mode, enabled: bool) -> io::Result<()> {
            self.operations.push((mode, enabled));
            if self.failures.contains(&(mode, enabled)) {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected I/O failure",
                ))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn terminal_both_streams_must_be_terminals_before_setup() {
        for (stdin, stdout) in [(false, false), (false, true), (true, false)] {
            assert_eq!(
                require_terminal(stdin, stdout).unwrap_err().kind(),
                io::ErrorKind::NotConnected
            );
        }
        assert!(require_terminal(true, true).is_ok());
    }

    #[test]
    fn terminal_normal_cleanup_reverses_setup_and_is_idempotent() {
        let modes = Modes::default();
        let mut terminal = FakeTerminal::default();
        modes.acquire(&mut terminal).unwrap();
        terminal.operations.clear();

        modes.restore(&mut terminal).unwrap();
        assert_eq!(
            terminal.operations,
            [
                (Mode::Paste, false),
                (Mode::HiddenCursor, false),
                (Mode::Alternate, false),
                (Mode::Raw, false),
            ]
        );
        terminal.operations.clear();
        modes.restore(&mut terminal).unwrap();
        assert!(terminal.operations.is_empty());
    }

    #[test]
    fn terminal_every_partial_failure_restores_attempted_modes_only() {
        for (failed_index, failed_mode) in Mode::ORDER.into_iter().enumerate() {
            let modes = Modes::default();
            let mut terminal = FakeTerminal {
                failures: vec![(failed_mode, true)],
                ..FakeTerminal::default()
            };
            assert!(modes.acquire(&mut terminal).is_err());
            terminal.operations.clear();

            modes.restore(&mut terminal).unwrap();
            let expected: Vec<_> = Mode::ORDER[..=failed_index]
                .iter()
                .rev()
                .map(|mode| (*mode, false))
                .collect();
            assert_eq!(terminal.operations, expected);
        }
    }

    #[test]
    fn terminal_cleanup_continues_after_failures_and_retries_only_failed_steps() {
        let modes = Modes::default();
        let mut terminal = FakeTerminal::default();
        modes.acquire(&mut terminal).unwrap();
        terminal.operations.clear();
        terminal.failures = vec![(Mode::Paste, false), (Mode::Alternate, false)];

        let error = modes.restore(&mut terminal).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(error.to_string().contains("bracketed paste"));
        assert!(error.to_string().contains("alternate screen"));
        assert_eq!(terminal.operations.last(), Some(&(Mode::Raw, false)));

        terminal.operations.clear();
        terminal.failures.clear();
        modes.restore(&mut terminal).unwrap();
        assert_eq!(
            terminal.operations,
            [(Mode::Paste, false), (Mode::Alternate, false)]
        );
    }
}
