//! Asura's llama.cpp server process (llamacpp-backend.md): the server plan,
//! the key pipe, the signal reset before `execve`, the LaunchAgent file and
//! the `launchctl` calls.
//!
//! The serve step and the CLI commands are one-shot processes; their waits
//! (a `launchctl` child) end on the child's exit or the person's interrupt.
//! No function here runs on the service reactor.
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The LaunchAgent label.
pub const LABEL: &str = "org.pidster.asura.llamacpp";
/// The key: 32 random bytes as 64 lower-case hex digits.
pub const KEY_HEX_BYTES: usize = 64;

/// What the serve step starts. Values come from the validated configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerPlan {
    pub binary: PathBuf,
    pub models: PathBuf,
    pub port: u16,
    pub context_length: u32,
    pub slots: u32,
    pub models_loaded: u32,
    /// 0 never sleeps: `--sleep-idle-seconds` is left out.
    pub idle_seconds: u32,
}

impl ServerPlan {
    /// The router's arguments; the key is read from `/dev/fd/<key_fd>`.
    pub fn arguments(&self, key_fd: i32) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![
            "--models-dir".into(),
            self.models.clone().into_os_string(),
            "--models-max".into(),
            self.models_loaded.to_string().into(),
            "--ctx-size".into(),
            self.context_length.to_string().into(),
            "--parallel".into(),
            self.slots.to_string().into(),
        ];
        if self.idle_seconds > 0 {
            args.push("--sleep-idle-seconds".into());
            args.push(self.idle_seconds.to_string().into());
        }
        args.extend(
            [
                "--no-ui",
                "--host",
                "127.0.0.1",
                "--port",
                &self.port.to_string(),
                "--api-key-file",
                &format!("/dev/fd/{key_fd}"),
            ]
            .map(OsString::from),
        );
        args
    }
}

/// The environment passed on: every `LLAMA_*` variable is removed, so none
/// can change the host, the port or the key.
pub fn environment<I: IntoIterator<Item = (OsString, OsString)>>(
    vars: I,
) -> Vec<(OsString, OsString)> {
    vars.into_iter()
        .filter(|(name, _)| !name.as_encoded_bytes().starts_with(b"LLAMA_"))
        .collect()
}

/// A pipe holding the key: the write end is written and closed at once,
/// the read end stays open across `execve`. 65 bytes is below `PIPE_BUF`,
/// so the write cannot block.
fn key_pipe(key: &[u8]) -> io::Result<OwnedFd> {
    if key.len() != KEY_HEX_BYTES || !key.iter().all(u8::is_ascii_hexdigit) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "key"));
    }
    let mut fds = [0; 2];
    // SAFETY: fds is a writable array of two descriptors.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe returned two new descriptors this function now owns.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // SAFETY: borrowed descriptor; sets close-on-exec on the write end only.
    if unsafe { libc::fcntl(write.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut writer = File::from(write);
    writer.write_all(key)?;
    writer.write_all(b"\n")?;
    drop(writer);
    Ok(read)
}

/// The command for `plan`, with the key pipe, the log on stdout and stderr,
/// the filtered environment and the signal reset (Wisp: a process keeps a
/// blocked signal mask across `execve`, and the router then ignored SIGTERM).
fn command(plan: &ServerPlan, key: &OwnedFd, log: &File) -> io::Result<Command> {
    let mut command = Command::new(&plan.binary);
    command
        .args(plan.arguments(key.as_raw_fd()))
        .env_clear()
        .envs(environment(std::env::vars_os()))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?));
    // SAFETY: the hook runs between fork (or before execve) and exec; it
    // calls only async-signal-safe functions on scalar arguments.
    unsafe {
        command.pre_exec(|| {
            let mut empty: libc::sigset_t = 0;
            libc::sigemptyset(&mut empty);
            if libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
            for signal in 1..32 {
                if signal != libc::SIGKILL && signal != libc::SIGSTOP {
                    libc::signal(signal, libc::SIG_DFL);
                }
            }
            Ok(())
        });
    }
    Ok(command)
}

/// Replaces this process with `llama-server` (the serve step). Returns only
/// on failure.
pub fn exec(plan: &ServerPlan, key: &[u8], log: &File) -> io::Error {
    let pipe = match key_pipe(key) {
        Ok(pipe) => pipe,
        Err(error) => return error,
    };
    match command(plan, &pipe, log) {
        Ok(mut command) => command.exec(),
        Err(error) => error,
    }
}

/// The test-only launch path (LC-I5): the same plan, key pipe, environment
/// and signal reset as [`exec`], started as a child in its own session so a
/// test can stop it. Tests never register a LaunchAgent.
#[cfg(feature = "test-support")]
pub fn spawn_for_test(
    plan: &ServerPlan,
    key: &[u8],
    log: &File,
) -> io::Result<std::process::Child> {
    let pipe = key_pipe(key)?;
    let mut command = command(plan, &pipe, log)?;
    command.process_group(0);
    let child = command.spawn();
    drop(pipe);
    child
}

/// The LaunchAgent file that runs `program llamacpp serve` (LCD14).
pub fn agent_plist(program: &Path) -> io::Result<String> {
    let program = program
        .to_str()
        .filter(|p| p.starts_with('/') && !p.contains(['<', '>', '&', '\n']))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "program path"))?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{program}</string>
		<string>llamacpp</string>
		<string>serve</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>ThrottleInterval</key>
	<integer>30</integer>
</dict>
</plist>
"#
    ))
}

/// The agent file's path in an account home.
pub fn agent_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// Runs `launchctl`. Production uses [`SystemLaunchctl`]; tests use a fake,
/// so no test registers an agent in the person's account.
pub trait Launchctl {
    /// Runs `launchctl <args>`; returns its exit status and standard output.
    fn run(&self, args: &[&OsStr]) -> io::Result<(i32, String)>;
}

/// `/bin/launchctl`, at most 64 KiB of output kept.
pub struct SystemLaunchctl;
impl Launchctl for SystemLaunchctl {
    fn run(&self, args: &[&OsStr]) -> io::Result<(i32, String)> {
        let output = Command::new("/bin/launchctl")
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()?;
        let text =
            String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(65_536)]).into_owned();
        Ok((output.status.code().unwrap_or(-1), text))
    }
}

fn target(uid: u32) -> String {
    format!("gui/{uid}/{LABEL}")
}

/// The agent's state as launchd reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentState {
    NotLoaded,
    /// Loaded; `pid` when a process runs, and the last exit status.
    Loaded {
        pid: Option<u32>,
        last_exit: Option<i32>,
    },
}

/// `launchctl print gui/<uid>/<label>`.
pub fn agent_state(launchctl: &dyn Launchctl, uid: u32) -> io::Result<AgentState> {
    let (status, text) = launchctl.run(&[OsStr::new("print"), OsStr::new(&target(uid))])?;
    if status != 0 {
        return Ok(AgentState::NotLoaded);
    }
    let field = |name: &str| {
        text.lines()
            .map(str::trim)
            .find_map(|line| {
                line.strip_prefix(name)?
                    .trim_start()
                    .strip_prefix('=')
                    .map(str::trim)
            })
            .map(str::to_owned)
    };
    Ok(AgentState::Loaded {
        pid: field("pid").and_then(|v| v.parse().ok()),
        last_exit: field("last exit code").and_then(|v| v.parse().ok()),
    })
}

/// What [`start`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Started {
    Registered,
    Restarted,
}

/// Writes the agent file and loads it, or restarts a loaded agent.
pub fn start(
    launchctl: &dyn Launchctl,
    uid: u32,
    home: &Path,
    program: &Path,
) -> io::Result<Started> {
    if matches!(agent_state(launchctl, uid)?, AgentState::Loaded { .. }) {
        restart(launchctl, uid)?;
        return Ok(Started::Restarted);
    }
    let path = agent_path(home);
    std::fs::create_dir_all(path.parent().expect("agent path has a parent"))?;
    let temporary = path.with_extension("plist.tmp");
    std::fs::write(&temporary, agent_plist(program)?)?;
    std::fs::rename(&temporary, &path)?;
    let (status, _) = launchctl.run(&[
        OsStr::new("bootstrap"),
        OsStr::new(&format!("gui/{uid}")),
        path.as_os_str(),
    ])?;
    if status != 0 {
        return Err(io::Error::other(format!(
            "launchctl bootstrap exited {status}"
        )));
    }
    Ok(Started::Registered)
}

/// `launchctl kickstart -k`: the router reads its folder only at start.
pub fn restart(launchctl: &dyn Launchctl, uid: u32) -> io::Result<()> {
    let (status, _) = launchctl.run(&[
        OsStr::new("kickstart"),
        OsStr::new("-k"),
        OsStr::new(&target(uid)),
    ])?;
    if status != 0 {
        return Err(io::Error::other(format!(
            "launchctl kickstart exited {status}"
        )));
    }
    Ok(())
}

/// Boots the agent out when loaded and removes its file. Returns whether
/// anything was loaded or present.
pub fn stop(launchctl: &dyn Launchctl, uid: u32, home: &Path) -> io::Result<bool> {
    let loaded = matches!(agent_state(launchctl, uid)?, AgentState::Loaded { .. });
    if loaded {
        let (status, _) = launchctl.run(&[OsStr::new("bootout"), OsStr::new(&target(uid))])?;
        if status != 0 {
            return Err(io::Error::other(format!(
                "launchctl bootout exited {status}"
            )));
        }
    }
    let path = agent_path(home);
    let present = match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e),
    };
    Ok(loaded || present)
}

/// Starts a TCP connection to `127.0.0.1:<port>` without blocking (the
/// relay's upstream, LC-S2). The caller polls for writability and then reads
/// `take_error` for the outcome; a full listen backlog cannot stall it.
pub fn connect_loopback(port: u16) -> io::Result<std::net::TcpStream> {
    // SAFETY: plain socket creation with scalar arguments.
    let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: socket returned a new descriptor this function now owns.
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: borrowed descriptor; sets close-on-exec.
    if unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    crate::nonblocking(socket.as_raw_fd()).map_err(io::Error::other)?;
    let one: libc::c_int = 1;
    // SAFETY: the option value points to one c_int for the call.
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&one as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    // SAFETY: sockaddr_in is plain data; every field is set below.
    let mut address: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    address.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
    address.sin_family = libc::AF_INET as libc::sa_family_t;
    address.sin_port = port.to_be();
    address.sin_addr.s_addr = u32::from(std::net::Ipv4Addr::LOCALHOST).to_be();
    // SAFETY: the address points to one initialized sockaddr_in of the given length.
    let result = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            (&address as *const libc::sockaddr_in).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error);
        }
    }
    Ok(std::net::TcpStream::from(socket))
}

/// 32 random bytes as 64 hex digits.
pub fn new_key() -> zeroize::Zeroizing<Vec<u8>> {
    let mut raw = zeroize::Zeroizing::new([0u8; 32]);
    crate::fill_random(&mut raw[..]);
    let mut key = zeroize::Zeroizing::new(Vec::with_capacity(KEY_HEX_BYTES));
    for byte in raw.iter() {
        key.push(b"0123456789abcdef"[usize::from(byte >> 4)]);
        key.push(b"0123456789abcdef"[usize::from(byte & 15)]);
    }
    key
}

#[cfg(test)]
mod tests;
