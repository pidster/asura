use super::{ShellError, protocol::Notice, spawn};
use crate::runtime::{open_at, stat_fd};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

// Child ownership loss is irreversible. A later PID observation cannot restore
// authority to signal that process group. Fault injection tests exercise this
// state without starting or signalling processes.
struct CleanupSafety {
    fence_valid: bool,
    last_clock: u64,
    deadline: u64,
}
impl CleanupSafety {
    fn new(deadline: u64) -> Self {
        Self {
            fence_valid: true,
            last_clock: deadline,
            deadline,
        }
    }
    fn sample_clock(&mut self, sample: Result<u64, ShellError>) -> (u64, bool) {
        let fault = sample.is_err();
        let now = sample.unwrap_or_else(|_| {
            self.last_clock
                .max(self.deadline)
                .saturating_add(100_000_000)
        });
        self.last_clock = now;
        (now, fault)
    }
    fn observe_wait(&mut self, result: i32, errno: Option<i32>) {
        if result < 0 && errno != Some(libc::EINTR) {
            self.fence_valid = false;
        }
    }
    fn kill_due(&self, clock_fault: bool, elapsed: u64, already_killed: bool) -> bool {
        self.fence_valid && !already_killed && (clock_fault || elapsed >= 500_000_000)
    }
}

/// Private entrypoint; inherited descriptors must pass validation before any effect.
pub fn run_guardian(jobhex: &str, deadline_ns: &str, command: &str) -> i32 {
    match guardian(jobhex, deadline_ns, command) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}
fn parse_id(s: &str) -> Result<[u8; 16], ShellError> {
    if s.len() != 32
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ShellError::Invalid);
    }
    let mut id = [0; 16];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| ShellError::Invalid)?;
    }
    if id == [0; 16] {
        return Err(ShellError::Invalid);
    }
    Ok(id)
}
fn descriptors() -> Result<Vec<OwnedFd>, ShellError> {
    for fd in 3..=9 {
        let st = stat_fd(fd).map_err(|_| ShellError::Denied)?;
        // SAFETY: inspect access mode before assuming descriptor ownership.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        let access = if fd == 3 || fd >= 7 {
            libc::O_RDONLY
        } else {
            libc::O_WRONLY
        };
        if flags < 0 || flags & libc::O_ACCMODE != access || st.st_uid != unsafe { libc::geteuid() }
        {
            return Err(ShellError::Denied);
        }
        if fd <= 6 {
            if st.st_mode & libc::S_IFMT != libc::S_IFIFO {
                return Err(ShellError::Denied);
            }
        } else if st.st_mode & libc::S_IFMT != libc::S_IFDIR || st.st_nlink == 0 {
            return Err(ShellError::Denied);
        }
        if (fd == 3 || fd == 4) && flags & libc::O_NONBLOCK == 0 {
            return Err(ShellError::Denied);
        }
        if fd == 9 && st.st_mode & 0o777 != 0o700 {
            return Err(ShellError::Denied);
        }
    }
    // SAFETY: this private invocation exclusively receives the validated FDs.
    Ok((3..=9)
        .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
        .collect())
}
fn pinned_path(fd: i32) -> Result<String, ShellError> {
    let path = spawn::path(fd)?;
    let identity = crate::ProjectIdentity::open(&path).map_err(|_| ShellError::Denied)?;
    let held = stat_fd(fd).map_err(|_| ShellError::Denied)?;
    if identity.device != held.st_dev as u64 || identity.inode != held.st_ino {
        return Err(ShellError::Denied);
    }
    Ok(path)
}
fn profile(project: &str, scratch: &str) -> Result<String, ShellError> {
    let quote = |s: &str| crate::git_observer::quoted(s).map_err(|_| ShellError::Denied);
    let roots = [
        "/bin",
        "/usr/bin",
        "/sbin",
        "/usr/sbin",
        "/usr/lib",
        "/usr/share",
        "/System",
        project,
        scratch,
    ];
    let mut filters = String::new();
    let mut ancestors = std::collections::BTreeSet::new();
    for root in roots {
        filters.push_str(&format!(" (subpath {})", quote(root)?));
        let mut p = Path::new(root).parent();
        while let Some(parent) = p {
            ancestors.insert(parent.to_string_lossy().into_owned());
            p = parent.parent();
        }
    }
    let mut metadata = String::new();
    for p in ancestors {
        metadata.push_str(&format!(" (literal {})", quote(&p)?));
    }
    Ok(format!(
        "(version 1)(deny default)(allow file-read* file-test-existence (literal \"/\"))(allow file-read* (literal \"/private/var/select/sh\"))(allow file-read-metadata (literal \"/private\") (literal \"/private/var\") (literal \"/private/var/select\"))(allow process-fork)(allow process-exec file-map-executable file-read*{filters})(allow file-read-metadata{metadata})(allow file-write* (subpath {}) (subpath {}))(allow sysctl-read)(allow file-read* file-write* (literal \"/dev/null\"))(allow file-read* (literal \"/dev/random\") (literal \"/dev/urandom\"))",
        quote(project)?,
        quote(scratch)?
    ))
}
fn notice(fd: i32, n: Notice, id: [u8; 16]) -> Result<(), ShellError> {
    let bytes = n.encode(id);
    // SAFETY: bounded atomic write (<PIPE_BUF) to private nonblocking status pipe.
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if written == 32 {
        Ok(())
    } else {
        Err(ShellError::Protocol)
    }
}
fn guardian(jobhex: &str, deadline_ns: &str, command: &str) -> Result<(), ShellError> {
    let id = parse_id(jobhex)?;
    if command.is_empty()
        || command.len() > 8192
        || command.contains('\0')
        || deadline_ns.len() > 20
    {
        return Err(ShellError::Invalid);
    }
    let deadline = deadline_ns
        .parse::<u64>()
        .map_err(|_| ShellError::Invalid)?;
    let now = spawn::monotonic_ns()?;
    if deadline <= now || deadline - now > 60_000_000_000 {
        return Err(ShellError::Deadline);
    }
    let descriptors = descriptors()?;
    // SAFETY: guardian is a single-purpose fresh child, before spawning command.
    unsafe {
        libc::umask(0o077);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let setup = (|| {
        let project = pinned_path(7)?;
        let cwd = pinned_path(8)?;
        let scratch = pinned_path(9)?;
        if cwd != project && !cwd.starts_with(&format!("{project}/")) {
            return Err(ShellError::Denied);
        }
        let profile = profile(&project, &scratch)?;
        if lease_reason(3)?.is_some() {
            return Err(ShellError::Cancelled);
        }
        if spawn::monotonic_ns()? >= deadline {
            return Err(ShellError::Deadline);
        }
        let null =
            open_at(libc::AT_FDCWD, c"/dev/null", libc::O_RDONLY, 0).map_err(|_| ShellError::Io)?;
        let args = vec![
            "/usr/bin/sandbox-exec".into(),
            "-p".into(),
            profile,
            "/bin/sh".into(),
            "-c".into(),
            command.into(),
        ];
        let env = vec![
            "PATH=/usr/bin:/bin:/usr/sbin:/sbin".into(),
            "LC_ALL=C".into(),
            "LANG=C".into(),
            format!("HOME={scratch}"),
            format!("TMPDIR={scratch}"),
        ];
        spawn::launch(
            Path::new("/usr/bin/sandbox-exec"),
            &args,
            &env,
            &[(null.as_raw_fd(), 0), (5, 1), (6, 2)],
            Some(8),
            true,
        )
    })();
    let pid = match setup {
        Ok(pid) => pid,
        Err(error) => {
            notice(
                4,
                Notice {
                    kind: 3,
                    reason: match error {
                        ShellError::Denied => 7,
                        ShellError::Unavailable => 6,
                        _ => 5,
                    },
                    exit_kind: 0,
                    cleanup: true,
                    value: 0,
                },
                id,
            )?;
            return Ok(());
        }
    };
    // Close guardian capture writers so EOF means command descriptors settled.
    let mut held: Vec<_> = descriptors.into_iter().map(Some).collect();
    held[2].take();
    held[3].take();
    // A broken status reader never releases ownership of the live command.
    let mut report_ok = notice(
        4,
        Notice {
            kind: 1,
            reason: 0,
            exit_kind: 0,
            cleanup: false,
            value: 0,
        },
        id,
    )
    .is_ok();
    let mut stopping = None;
    let mut reason = 0;
    let mut killed = false;
    let mut reported_unknown = false;
    let mut safety = CleanupSafety::new(deadline);
    loop {
        let (now, clock_fault) = safety.sample_clock(spawn::monotonic_ns());
        // SAFETY: WNOWAIT keeps the direct child PID allocated as a group-signal fence.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let waiting = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        safety.observe_wait(
            waiting,
            if waiting < 0 {
                std::io::Error::last_os_error().raw_os_error()
            } else {
                None
            },
        );
        if !safety.fence_valid {
            report_ok = false;
        }
        let exited = waiting == 0 && info.si_pid == pid;
        if stopping.is_none() {
            let lease = lease_reason(3).unwrap_or(Some(4));
            if now >= deadline || lease.is_some() || exited || !report_ok {
                reason = if now >= deadline {
                    1
                } else {
                    lease.unwrap_or(0)
                };
                stopping = Some(now);
                // SAFETY: leader has never been reaped; its PID fences this owned group.
                if safety.fence_valid {
                    unsafe {
                        libc::kill(-pid, libc::SIGTERM);
                    }
                }
            }
        }
        if let Some(start) = stopping {
            if safety.kill_due(clock_fault, now.saturating_sub(start), killed) {
                if safety.fence_valid {
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                }
                killed = true;
            }
            if safety.fence_valid && exited && group_only_leader(pid).unwrap_or(false) {
                let mut status = 0;
                // SAFETY: our zombie leader is reaped only after all group signals end.
                let reaped = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                if reaped == pid {
                    let (exit_kind, value) = if libc::WIFEXITED(status) {
                        (1, libc::WEXITSTATUS(status) as u32)
                    } else {
                        (2, libc::WTERMSIG(status) as u32)
                    };
                    if report_ok {
                        notice(
                            4,
                            Notice {
                                kind: 2,
                                reason,
                                exit_kind,
                                cleanup: true,
                                value,
                            },
                            id,
                        )?;
                    }
                    return if report_ok {
                        Ok(())
                    } else {
                        Err(ShellError::Protocol)
                    };
                }
            }
            if now.saturating_sub(start) >= 2_500_000_000 && !reported_unknown {
                if report_ok {
                    report_ok = notice(
                        4,
                        Notice {
                            kind: 4,
                            reason: 9,
                            exit_kind: 0,
                            cleanup: false,
                            value: 0,
                        },
                        id,
                    )
                    .is_ok();
                }
                reported_unknown = true;
            }
        }
        // One bounded polling wait; lease closure and absolute expiry are rechecked.
        let mut pfd = libc::pollfd {
            fd: if stopping.is_none() { 3 } else { -1 },
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe {
            libc::poll(&mut pfd, 1, 100);
        }
    }
}
fn lease_reason(fd: i32) -> Result<Option<u8>, ShellError> {
    let mut b = [0u8; 1];
    let n = unsafe { libc::read(fd, b.as_mut_ptr().cast(), 1) };
    if n == 0 {
        Ok(Some(4))
    } else if n < 0 && spawn::would_block() {
        Ok(None)
    } else {
        Err(ShellError::Protocol)
    }
}
fn group_only_leader(pid: libc::pid_t) -> Result<bool, ShellError> {
    let mut members = [0i32; 256];
    // SAFETY: bounded complete PID buffer. PROC_PGRP_ONLY=2 from installed SDK.
    let bytes = unsafe {
        libc::proc_listpids(
            2,
            pid as u32,
            members.as_mut_ptr().cast(),
            std::mem::size_of_val(&members) as i32,
        )
    };
    if bytes < 0
        || bytes as usize >= std::mem::size_of_val(&members)
        || !(bytes as usize).is_multiple_of(4)
    {
        return Err(ShellError::CleanupUnconfirmed);
    }
    Ok(members[..bytes as usize / 4]
        .iter()
        .all(|member| *member == pid))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lost_child_fence_cannot_be_restored_by_recycled_pid_observation() {
        let mut safety = CleanupSafety::new(1_000);
        safety.observe_wait(-1, Some(libc::EINTR));
        assert!(safety.fence_valid);
        safety.observe_wait(-1, Some(libc::ECHILD));
        assert!(!safety.fence_valid);
        safety.observe_wait(0, None);
        assert!(!safety.fence_valid);
        assert!(!safety.kill_due(true, u64::MAX, false));
    }
    #[test]
    fn repeated_clock_failure_advances_cleanup_and_kills_only_owned_group() {
        let mut safety = CleanupSafety::new(1_000);
        let (first, fault) = safety.sample_clock(Err(ShellError::Io));
        assert!(first >= 1_000);
        assert!(fault);
        assert!(safety.kill_due(fault, 0, false));
        let (second, fault) = safety.sample_clock(Err(ShellError::Io));
        assert!(second > first);
        assert!(fault);
        assert!(!safety.kill_due(fault, 0, true));
        assert!(!safety.kill_due(false, 499_999_999, false));
        assert!(safety.kill_due(false, 500_000_000, false));
        safety.observe_wait(-1, Some(libc::EINVAL));
        assert!(!safety.kill_due(fault, second - first, false));
    }
}
