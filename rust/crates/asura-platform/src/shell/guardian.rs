use super::{MAX_COMMAND_BYTES, ShellError, protocol::Notice, spawn};
use crate::runtime::{open_at, stat_fd};
use crate::secrets::rewrite::MAX_SECRET_VARIABLES;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use zeroize::Zeroizing;

const SECRET_PREFIX: &[u8] = b"ASURA_SECRET_";
/// CoreFoundation's load-time initializer sets this variable in every process
/// that links it, before `main`, when the spawner did not. The guardian skips
/// it and never passes it on; every other name is refused.
const CF_TEXT_ENCODING: &[u8] = b"__CF_USER_TEXT_ENCODING=";
/// Bound on the preamble. The longest, for all 16 numbers, is 1,254 bytes, so
/// the `sh -c` script is at most the 8192-byte command plus this.
const PREAMBLE_LIMIT: usize = 1280;

/// One validated secret variable from the guardian's own environment: its
/// number and the whole `ASURA_SECRET_<n>=<value>` entry, zeroed on drop.
struct SecretEntry {
    number: u8,
    entry: Zeroizing<Vec<u8>>,
}

/// Validate the guardian's environment (SDU-13). Every entry must be
/// `ASURA_SECRET_<n>=<value>` with a canonical decimal n of 1–16, no number
/// twice, no NUL, and at most 8192 value bytes in total. Any other entry
/// refuses the whole set. Returns the entries ordered by number.
fn secret_environment<'a>(
    entries: impl IntoIterator<Item = &'a [u8]>,
) -> Result<Vec<SecretEntry>, ShellError> {
    let mut accepted: Vec<SecretEntry> = Vec::new();
    let mut total = 0usize;
    for entry in entries {
        if entry.starts_with(CF_TEXT_ENCODING) {
            continue;
        }
        if accepted.len() == MAX_SECRET_VARIABLES || entry.contains(&0) {
            return Err(ShellError::Invalid);
        }
        let split = entry
            .iter()
            .position(|b| *b == b'=')
            .ok_or(ShellError::Invalid)?;
        let (name, value) = (&entry[..split], &entry[split + 1..]);
        let digits = name
            .strip_prefix(SECRET_PREFIX)
            .ok_or(ShellError::Invalid)?;
        if digits.is_empty()
            || digits.len() > 2
            || digits[0] == b'0'
            || !digits.iter().all(u8::is_ascii_digit)
        {
            return Err(ShellError::Invalid);
        }
        let number = digits.iter().fold(0u8, |n, d| n * 10 + (d - b'0'));
        if usize::from(number) > MAX_SECRET_VARIABLES
            || accepted.iter().any(|seen| seen.number == number)
        {
            return Err(ShellError::Invalid);
        }
        total += value.len();
        if total > MAX_COMMAND_BYTES {
            return Err(ShellError::Invalid);
        }
        accepted.push(SecretEntry {
            number,
            entry: Zeroizing::new(entry.to_vec()),
        });
    }
    accepted.sort_by_key(|secret| secret.number);
    Ok(accepted)
}

/// Validate this process's own environment, then erase it (owner decision
/// SDI-09, 2026-10-15).
///
/// The strings that `environ` points to at `main` are the ones the kernel
/// placed on the initial stack, and `KERN_PROCARGS2` (`ps -E`) reads that
/// memory. After the accepted values are copied into `Zeroizing` buffers, every
/// walked entry other than the CoreFoundation one is overwritten with zero
/// bytes, name and value alike, whether the set was accepted or refused. Each
/// string becomes empty, so the kernel view ends its environment list there.
/// Entries past the walk bound are not erased; such a set is always refused
/// and the guardian then exits after its notice.
fn own_secret_environment() -> Result<Vec<SecretEntry>, ShellError> {
    use zeroize::Zeroize;
    let mut walked: Vec<(*mut u8, usize)> = Vec::new();
    // SAFETY: _NSGetEnviron returns this process's `environ`, a NULL-terminated
    // array of NUL-terminated strings. The guardian never changes its
    // environment and runs one thread, so the array and strings stay valid and
    // unchanged while read here. The walk stops two entries past the limit:
    // one skipped CoreFoundation entry and one that refuses the set.
    unsafe {
        let environ = *libc::_NSGetEnviron();
        if !environ.is_null() {
            let mut cursor = environ;
            while !(*cursor).is_null() && walked.len() <= MAX_SECRET_VARIABLES + 1 {
                let length = std::ffi::CStr::from_ptr(*cursor).to_bytes().len();
                walked.push(((*cursor).cast::<u8>(), length));
                cursor = cursor.add(1);
            }
        }
    }
    // SAFETY: each pointer and length came from the walk above; no shared
    // slice outlives this call, because the result holds owned copies.
    let result = secret_environment(
        walked
            .iter()
            .map(|(pointer, length)| unsafe { std::slice::from_raw_parts(*pointer, *length) }),
    );
    for (pointer, length) in walked {
        // SAFETY: the string is writable process memory of `length` bytes
        // before its NUL, and no reference to it remains. Writing zeros inside
        // that range keeps a valid (empty) C string; nothing reads the
        // environment afterwards, since `sh` gets an explicit array.
        let entry = unsafe { std::slice::from_raw_parts_mut(pointer, length) };
        if !entry.starts_with(CF_TEXT_ENCODING) {
            entry.zeroize();
        }
    }
    result
}

/// The fixed preamble. Each variable that `sh` imported from its environment
/// is exported; `unset` and reassignment leave an ordinary shell variable with
/// the same value, which the command expands but children do not inherit.
/// Assignment does no field splitting or globbing, so the value is exact. The
/// text holds names only.
fn preamble(numbers: impl IntoIterator<Item = u8>) -> String {
    let mut text = String::new();
    for n in numbers {
        text.push_str(&format!(
            "__asura_t=$ASURA_SECRET_{n}; unset ASURA_SECRET_{n}; ASURA_SECRET_{n}=$__asura_t; "
        ));
    }
    if !text.is_empty() {
        text.push_str("unset __asura_t; ");
    }
    text
}

// Child ownership loss is irreversible. A later PID observation cannot restore
// authority to signal that process group. Fault injection tests exercise this
// state without starting or signalling processes.
//
// The clock-fault fallback is seeded with the guardian's first clock sample,
// not a deadline (J1): a failed sample advances 100 ms from the last one.
struct CleanupSafety {
    fence_valid: bool,
    last_clock: u64,
}
impl CleanupSafety {
    fn new(first_sample: u64) -> Self {
        Self {
            fence_valid: true,
            last_clock: first_sample,
        }
    }
    fn sample_clock(&mut self, sample: Result<u64, ShellError>) -> (u64, bool) {
        let fault = sample.is_err();
        let now = sample.unwrap_or_else(|_| self.last_clock.saturating_add(100_000_000));
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

/// The deadline argument for no-deadline mode (J1-Q3): the command runs until
/// it ends, its lease closes or its leader exits.
pub(super) const NO_DEADLINE: &str = "none";

/// Parses the deadline argument: `none`, or monotonic nanoseconds no more
/// than 60 s ahead of `now` (the bounded mode kept for qualification).
fn parse_deadline(deadline_ns: &str, now: u64) -> Result<Option<u64>, ShellError> {
    if deadline_ns == NO_DEADLINE {
        return Ok(None);
    }
    if deadline_ns.is_empty()
        || deadline_ns.len() > 20
        || !deadline_ns.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(ShellError::Invalid);
    }
    let deadline = deadline_ns
        .parse::<u64>()
        .map_err(|_| ShellError::Invalid)?;
    if deadline <= now || deadline - now > 60_000_000_000 {
        return Err(ShellError::Deadline);
    }
    Ok(Some(deadline))
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
    for fd in 3..=10 {
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
        if fd >= 9 && st.st_mode & 0o777 != 0o700 {
            return Err(ShellError::Denied);
        }
    }
    // SAFETY: this private invocation exclusively receives the validated FDs.
    Ok((3..=10)
        .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
        .collect())
}
fn pinned_path(fd: i32) -> Result<String, ShellError> {
    let path = spawn::path(fd)?;
    let (device, inode) =
        crate::project_identity::pinned_identity(&path).map_err(|_| ShellError::Denied)?;
    let held = stat_fd(fd).map_err(|_| ShellError::Denied)?;
    if device != held.st_dev as u64 || inode != held.st_ino {
        return Err(ShellError::Denied);
    }
    Ok(path)
}
/// `path` is `root` or lies beneath it.
fn within(path: &str, root: &str) -> bool {
    path == root || path.starts_with(&format!("{}/", root.trim_end_matches('/')))
}
/// A path check inside the guardian. Storage's identity check is the authority.
fn separate_from_state(project: &str, scratch: &str, asura: &str) -> Result<(), ShellError> {
    if !scratch.starts_with(&format!("{asura}/tmp/"))
        || within(project, asura)
        || within(asura, project)
    {
        return Err(ShellError::Denied);
    }
    Ok(())
}
fn profile(project: &str, scratch: &str, asura: &str) -> Result<String, ShellError> {
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
    // The last matching rule wins: deny the Asura home, then re-allow scratch.
    let state = format!(
        "(deny file-read* file-write* process-exec file-map-executable (subpath {asura}))(allow process-exec file-map-executable file-read* (subpath {scratch}))(allow file-write* (subpath {scratch}))(allow file-read-metadata (literal {asura}) (literal {tmp}))",
        asura = quote(asura)?,
        scratch = quote(scratch)?,
        tmp = quote(&format!("{asura}/tmp"))?,
    );
    Ok(format!(
        "(version 1)(deny default)(allow file-read* file-test-existence (literal \"/\"))(allow file-read* (literal \"/private/var/select/sh\"))(allow file-read-metadata (literal \"/private\") (literal \"/private/var\") (literal \"/private/var/select\"))(allow process-fork)(allow process-exec file-map-executable file-read*{filters})(allow file-read-metadata{metadata})(allow file-write* (subpath {}) (subpath {}))(allow sysctl-read)(allow file-read* file-write* (literal \"/dev/null\"))(allow file-read* (literal \"/dev/random\") (literal \"/dev/urandom\")){state}",
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
    if command.is_empty() || command.len() > MAX_COMMAND_BYTES || command.contains('\0') {
        return Err(ShellError::Invalid);
    }
    let first_sample = spawn::monotonic_ns()?;
    let deadline = parse_deadline(deadline_ns, first_sample)?;
    let descriptors = descriptors()?;
    // SAFETY: guardian is a single-purpose fresh child, before spawning command.
    unsafe {
        libc::umask(0o077);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let setup = (|| {
        let secrets = own_secret_environment()?;
        let project = pinned_path(7)?;
        let cwd = pinned_path(8)?;
        let scratch = pinned_path(9)?;
        let asura = pinned_path(10)?;
        if cwd != project && !cwd.starts_with(&format!("{project}/")) {
            return Err(ShellError::Denied);
        }
        separate_from_state(&project, &scratch, &asura)?;
        let profile = profile(&project, &scratch, &asura)?;
        if lease_reason(3)?.is_some() {
            return Err(ShellError::Cancelled);
        }
        // No-deadline mode skips the setup deadline check (J1-Q4).
        if let Some(deadline) = deadline
            && spawn::monotonic_ns()? >= deadline
        {
            return Err(ShellError::Deadline);
        }
        let null =
            open_at(libc::AT_FDCWD, c"/dev/null", libc::O_RDONLY, 0).map_err(|_| ShellError::Io)?;
        // The preamble is added here, after both 8192-byte command checks;
        // the script is bounded by the command limit plus PREAMBLE_LIMIT.
        let script = format!(
            "{}{command}",
            preamble(secrets.iter().map(|secret| secret.number))
        );
        if script.len() > MAX_COMMAND_BYTES + PREAMBLE_LIMIT {
            return Err(ShellError::Invalid);
        }
        let args = vec![
            "/usr/bin/sandbox-exec".into(),
            "-p".into(),
            profile,
            "/bin/sh".into(),
            "-c".into(),
            script,
        ];
        let fixed = [
            "PATH=/usr/bin:/bin:/usr/sbin:/sbin".to_owned(),
            "LC_ALL=C".into(),
            "LANG=C".into(),
            format!("HOME={scratch}"),
            format!("TMPDIR={scratch}"),
        ];
        let env: Vec<&[u8]> = fixed
            .iter()
            .map(|entry| entry.as_bytes())
            .chain(secrets.iter().map(|secret| secret.entry.as_slice()))
            .collect();
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
    let mut safety = CleanupSafety::new(first_sample);
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
            // No-deadline mode never stops with reason 1; lease closure
            // and leader exit still stop the group.
            let expired = deadline.is_some_and(|deadline| now >= deadline);
            if expired || lease.is_some() || exited || !report_ok {
                reason = if expired { 1 } else { lease.unwrap_or(0) };
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
    fn profile_denies_state_after_existing_rules_then_restores_scratch() {
        let text = profile("/p", "/h/.asura/tmp/shell-1", "/h/.asura").unwrap();
        let deny = text
            .find("(deny file-read* file-write* process-exec file-map-executable (subpath \"/h/.asura\"))")
            .expect("state deny");
        let exec = text
            .rfind("(allow process-exec file-map-executable file-read* (subpath \"/h/.asura/tmp/shell-1\"))")
            .expect("scratch exec");
        let write = text
            .rfind("(allow file-write* (subpath \"/h/.asura/tmp/shell-1\"))")
            .expect("scratch write");
        let metadata = text
            .find("(allow file-read-metadata (literal \"/h/.asura\") (literal \"/h/.asura/tmp\"))")
            .expect("metadata literals");
        assert!(
            text.rfind("(allow file-read* (literal \"/dev/random\")")
                .unwrap()
                < deny
        );
        assert!(deny < exec && exec < write && write < metadata);
        assert!(text.ends_with(")"));
        let quoted = profile("/p", "/h/.as\"ura/tmp/s", "/h/.as\"ura").unwrap();
        assert!(quoted.contains("(subpath \"/h/.as\\\"ura\")"));
    }
    fn numbers(entries: &[&[u8]]) -> Result<Vec<u8>, ShellError> {
        secret_environment(entries.iter().copied())
            .map(|accepted| accepted.iter().map(|s| s.number).collect())
    }
    /// SDU-13: each refusal becomes the setup failure notice in `guardian`.
    #[test]
    fn secret_environment_rejects_names_numbers_duplicates_nul_and_size() {
        assert_eq!(numbers(&[]), Ok(vec![]));
        assert_eq!(
            numbers(&[
                b"__CF_USER_TEXT_ENCODING=0x1F5:0x0:0x0",
                b"ASURA_SECRET_1=a"
            ]),
            Ok(vec![1]),
            "the CoreFoundation entry is skipped"
        );
        assert_eq!(
            numbers(&[
                b"ASURA_SECRET_16=b",
                b"ASURA_SECRET_1=a=b",
                b"ASURA_SECRET_2="
            ]),
            Ok(vec![1, 2, 16])
        );
        let all: Vec<Vec<u8>> = (1..=16)
            .map(|n| {
                let mut e = format!("ASURA_SECRET_{n}=").into_bytes();
                e.extend_from_slice(&[b'v'; 512]);
                e
            })
            .collect();
        let all: Vec<&[u8]> = all.iter().map(Vec::as_slice).collect();
        assert_eq!(numbers(&all).map(|n| n.len()), Ok(16), "8192 bytes");
        let mut big = b"ASURA_SECRET_1=".to_vec();
        big.extend_from_slice(&[b'v'; 8193]);
        let mut half = b"ASURA_SECRET_2=".to_vec();
        half.extend_from_slice(&[b'v'; 4097]);
        let mut other_half = b"ASURA_SECRET_3=".to_vec();
        other_half.extend_from_slice(&[b'v'; 4096]);
        for (label, entries) in [
            ("other name", vec![&b"PATH=/bin"[..]]),
            (
                "other name after valid",
                vec![&b"ASURA_SECRET_1=a"[..], b"HOME=/x"],
            ),
            ("prefix only", vec![&b"ASURA_SECRET_=a"[..]]),
            ("no separator", vec![&b"ASURA_SECRET_1"[..]]),
            ("lower case", vec![&b"asura_secret_1=a"[..]]),
            ("CF prefix", vec![&b"__CF_USER_TEXT_ENCODINGX=a"[..]]),
            ("number 0", vec![&b"ASURA_SECRET_0=a"[..]]),
            ("number 17", vec![&b"ASURA_SECRET_17=a"[..]]),
            ("leading zero", vec![&b"ASURA_SECRET_01=a"[..]]),
            ("three digits", vec![&b"ASURA_SECRET_100=a"[..]]),
            ("sign", vec![&b"ASURA_SECRET_+1=a"[..]]),
            (
                "duplicate",
                vec![&b"ASURA_SECRET_1=a"[..], b"ASURA_SECRET_1=a"],
            ),
            ("NUL", vec![&b"ASURA_SECRET_1=a\0b"[..]]),
            ("over 8192 bytes", vec![big.as_slice()]),
            (
                "over 8192 in total",
                vec![half.as_slice(), other_half.as_slice()],
            ),
        ] {
            assert_eq!(numbers(&entries), Err(ShellError::Invalid), "{label}");
        }
        let mut seventeen = all.clone();
        seventeen.push(b"ASURA_SECRET_1=a");
        assert_eq!(numbers(&seventeen), Err(ShellError::Invalid), "17 entries");
    }
    #[test]
    fn preamble_holds_names_only_and_is_bounded() {
        assert_eq!(preamble([]), "");
        assert_eq!(
            preamble([1, 12]),
            "__asura_t=$ASURA_SECRET_1; unset ASURA_SECRET_1; ASURA_SECRET_1=$__asura_t; \
             __asura_t=$ASURA_SECRET_12; unset ASURA_SECRET_12; ASURA_SECRET_12=$__asura_t; \
             unset __asura_t; "
        );
        let longest = preamble(1..=16);
        assert_eq!(longest.len(), 1254);
        assert!(longest.len() <= PREAMBLE_LIMIT);
        assert!(!longest.contains('\n'), "the command keeps its first line");
    }
    #[test]
    fn guardian_path_check_rejects_every_overlap() {
        let asura = "/h/.asura";
        let scratch = "/h/.asura/tmp/shell-1";
        assert!(separate_from_state("/h/project", scratch, asura).is_ok());
        assert!(separate_from_state("/h/.asura-other", scratch, asura).is_ok());
        for project in ["/h/.asura", "/h/.asura/run", "/h", "/"] {
            assert_eq!(
                separate_from_state(project, scratch, asura),
                Err(ShellError::Denied),
                "{project}"
            );
        }
        assert_eq!(
            separate_from_state("/h/project", "/h/project/tmp/x", asura),
            Err(ShellError::Denied)
        );
        assert_eq!(
            separate_from_state("/h/project", "/h/.asura/tmpx/x", asura),
            Err(ShellError::Denied)
        );
    }
    /// Defence in depth: even a project that contains the home cannot reach it
    /// under either spelling, while scratch stays usable.
    #[test]
    fn profile_denies_state_inside_a_containing_project() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::path::PathBuf::from(format!(
            "/private/tmp/as-sh-g-{}-{}",
            std::process::id(),
            crate::random_id()[..6]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        struct Remove(std::path::PathBuf);
        impl Drop for Remove {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        std::fs::create_dir(&home).unwrap();
        let _remove = Remove(home.clone());
        let asura = home.join(".asura");
        let scratch = asura.join("tmp/shell-1");
        std::fs::create_dir_all(&scratch).unwrap();
        for dir in [&home, &asura, &scratch] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::write(asura.join("secret"), b"state").unwrap();
        std::fs::write(home.join("visible"), b"project").unwrap();
        let text = profile(
            home.to_str().unwrap(),
            scratch.to_str().unwrap(),
            asura.to_str().unwrap(),
        )
        .unwrap();
        let run = |command: &str| {
            std::process::Command::new("/usr/bin/sandbox-exec")
                .args(["-p", &text, "/bin/sh", "-c", command])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .output()
                .unwrap()
                .status
                .success()
        };
        let firmlinked =
            std::path::Path::new("/System/Volumes/Data").join(asura.strip_prefix("/").unwrap());
        assert!(run(&format!(
            "/bin/cat '{}' >/dev/null",
            home.join("visible").display()
        )));
        for root in [&asura, &firmlinked] {
            let root = root.display();
            assert!(
                !run(&format!("/bin/cat '{root}/secret' >/dev/null")),
                "{root}"
            );
            assert!(!run(&format!("printf x > '{root}/written'")), "{root}");
        }
        let scratch = scratch.display();
        assert!(run(&format!(
            "printf x > '{scratch}/f' && /bin/cat '{scratch}/f' >/dev/null"
        )));
        assert!(!asura.join("written").exists());
    }
    /// J1T-05 unit: `none` selects no-deadline mode; a numeric deadline keeps
    /// the 60 s rule; anything else is invalid.
    #[test]
    fn j1t_05_guardian_deadline_argument_parsing() {
        let now = 1_000_000_000;
        assert_eq!(parse_deadline("none", now), Ok(None));
        assert_eq!(
            parse_deadline(&(now + 60_000_000_000).to_string(), now),
            Ok(Some(now + 60_000_000_000))
        );
        assert_eq!(
            parse_deadline(&(now + 60_000_000_001).to_string(), now),
            Err(ShellError::Deadline)
        );
        assert_eq!(
            parse_deadline(&now.to_string(), now),
            Err(ShellError::Deadline)
        );
        for bad in [
            "",
            "None",
            "NONE",
            "-1",
            "+5",
            "1e9",
            "123456789012345678901",
            " none",
        ] {
            assert_eq!(
                parse_deadline(bad, now),
                Err(ShellError::Invalid),
                "{bad:?}"
            );
        }
    }
    /// J1T-05 unit: without a deadline the clock-fault fallback is seeded
    /// with the first sample and advances 100 ms from the last one.
    #[test]
    fn j1t_05_cleanup_safety_without_a_deadline() {
        let mut safety = CleanupSafety::new(5_000_000_000);
        assert_eq!(
            safety.sample_clock(Ok(5_100_000_000)),
            (5_100_000_000, false)
        );
        assert_eq!(
            safety.sample_clock(Err(ShellError::Io)),
            (5_200_000_000, true)
        );
        assert_eq!(
            safety.sample_clock(Err(ShellError::Io)),
            (5_300_000_000, true)
        );
        let mut fresh = CleanupSafety::new(7);
        assert_eq!(fresh.sample_clock(Err(ShellError::Io)), (100_000_007, true));
    }
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
