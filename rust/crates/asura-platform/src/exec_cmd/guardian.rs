use super::escapee::{self, EnvMarker, FREEZE_ROUND_LIMIT, Gate, GateFacts, Libproc, Sweep, Tally};
use super::protocol::{
    KIND_ESCAPEE_REPORT, KIND_LEADER_EXITED, LEASE_KILL, LEASE_TERM, Notice, Profile,
    REASON_CANCEL, REASON_CLEANUP_FAILED, REASON_ESCAPEE_TERMINATED, REASON_ESCAPEE_UNTERMINATED,
    REASON_KILL, REASON_LEASE_ENDED,
};
use super::{ExecCmdError, MAX_COMMAND_BYTES, person, spawn};
use crate::runtime::{open_at, stat_fd};
use crate::secrets::rewrite::{MAX_SECRET_VARIABLES, SECRET_COMMAND_NAME, WHOLE_COMMAND};
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

/// A secret variable's name: `ASURA_SECRET_<n>`, or the whole-command
/// variable `ASURA_SECRET_COMMAND` (SO5).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum SecretName {
    Numbered(u8),
    Command,
}
impl std::fmt::Display for SecretName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Numbered(n) => write!(f, "ASURA_SECRET_{n}"),
            Self::Command => f.write_str(SECRET_COMMAND_NAME),
        }
    }
}
/// One validated secret variable from the guardian's own environment: its
/// name and the whole `<name>=<value>` entry, zeroed on drop.
struct SecretEntry {
    name: SecretName,
    entry: Zeroizing<Vec<u8>>,
}

/// Validate the guardian's environment (SDU-13, SDU-17). Every entry must be
/// `ASURA_SECRET_<n>=<value>` with a canonical decimal n of 1–16, no number
/// twice, no NUL, and at most 8192 value bytes in total; or the single entry
/// `ASURA_SECRET_COMMAND=<value>` with 1–8192 value bytes and no other
/// secret variable. Any other entry refuses the whole set. Returns the
/// entries ordered by number.
fn secret_environment<'a>(
    entries: impl IntoIterator<Item = &'a [u8]>,
) -> Result<Vec<SecretEntry>, ExecCmdError> {
    let mut accepted: Vec<SecretEntry> = Vec::new();
    let mut total = 0usize;
    for entry in entries {
        if entry.starts_with(CF_TEXT_ENCODING) {
            continue;
        }
        if accepted.len() == MAX_SECRET_VARIABLES || entry.contains(&0) {
            return Err(ExecCmdError::Invalid);
        }
        let split = entry
            .iter()
            .position(|b| *b == b'=')
            .ok_or(ExecCmdError::Invalid)?;
        let (name, value) = (&entry[..split], &entry[split + 1..]);
        let name = if name == SECRET_COMMAND_NAME.as_bytes() {
            if value.is_empty() {
                return Err(ExecCmdError::Invalid);
            }
            SecretName::Command
        } else {
            let digits = name
                .strip_prefix(SECRET_PREFIX)
                .ok_or(ExecCmdError::Invalid)?;
            if digits.is_empty()
                || digits.len() > 2
                || digits[0] == b'0'
                || !digits.iter().all(u8::is_ascii_digit)
            {
                return Err(ExecCmdError::Invalid);
            }
            let number = digits.iter().fold(0u8, |n, d| n * 10 + (d - b'0'));
            if usize::from(number) > MAX_SECRET_VARIABLES {
                return Err(ExecCmdError::Invalid);
            }
            SecretName::Numbered(number)
        };
        // The whole-command variable is accepted only alone (SO5).
        if accepted.iter().any(|seen| {
            seen.name == name || seen.name == SecretName::Command || name == SecretName::Command
        }) {
            return Err(ExecCmdError::Invalid);
        }
        total += value.len();
        if total > MAX_COMMAND_BYTES {
            return Err(ExecCmdError::Invalid);
        }
        accepted.push(SecretEntry {
            name,
            entry: Zeroizing::new(entry.to_vec()),
        });
    }
    accepted.sort_by_key(|secret| secret.name);
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
fn own_secret_environment() -> Result<Vec<SecretEntry>, ExecCmdError> {
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
/// text holds names only. It covers `ASURA_SECRET_COMMAND` the same way, so
/// `eval` reads an unexported variable (SO5).
fn preamble(names: impl IntoIterator<Item = SecretName>) -> String {
    let mut text = String::new();
    for name in names {
        text.push_str(&format!(
            "__asura_t=${name}; unset {name}; {name}=$__asura_t; "
        ));
    }
    if !text.is_empty() {
        text.push_str("unset __asura_t; ");
    }
    text
}

/// The `!!` job marker, `ASURA_JOB_<jobhex>=1`, with the 32 lowercase hex
/// digits of the job ID (escapee-control.md, the `!!` unsandboxed job).
fn job_marker(jobhex: &str) -> String {
    format!("ASURA_JOB_{jobhex}=1")
}
/// The path names an executable regular file (`access(X_OK)`).
fn executable(path: &str) -> bool {
    let Ok(path) = std::ffi::CString::new(path) else {
        return false;
    };
    // SAFETY: a NUL-terminated path; access only reads metadata.
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}
/// What the guardian spawns: executable, argv and envp. Environment entries
/// are copies in zeroizing memory.
struct CommandLine {
    executable: &'static str,
    args: Vec<String>,
    env: Vec<Zeroizing<Vec<u8>>>,
}
/// The command line for each profile (PC-U46). `model-1`: `sandbox-exec -p
/// PROFILE /bin/sh -c SCRIPT` with the fixed model environment; `person-1`:
/// the same with the person profile and the FD 11 entries; `person-outside-1`:
/// `/bin/sh -c SCRIPT` with no `sandbox-exec`, the FD 11 entries, then the
/// job marker. The secret variables come last in every case. The FD 11
/// entries apply only here, never to the guardian's own environment.
#[allow(clippy::too_many_arguments)]
fn command_line(
    profile: Profile,
    sandbox_exec: &'static str,
    sandbox: Option<String>,
    script: String,
    scratch: &str,
    marker: &str,
    frame: &[Zeroizing<Vec<u8>>],
    secrets: &[SecretEntry],
) -> CommandLine {
    let owned = |entry: &[u8]| Zeroizing::new(entry.to_vec());
    let mut env: Vec<Zeroizing<Vec<u8>>> = match profile {
        Profile::Model1 => [
            "PATH=/usr/bin:/bin:/usr/sbin:/sbin".to_owned(),
            "LC_ALL=C".into(),
            "LANG=C".into(),
            format!("HOME={scratch}"),
            format!("TMPDIR={scratch}"),
        ]
        .iter()
        .map(|entry| owned(entry.as_bytes()))
        .collect(),
        Profile::Person1 | Profile::PersonOutside1 => frame.iter().map(|e| owned(e)).collect(),
    };
    if profile == Profile::PersonOutside1 {
        env.push(owned(marker.as_bytes()));
    }
    env.extend(secrets.iter().map(|secret| owned(&secret.entry)));
    let shell = ["/bin/sh".to_owned(), "-c".into(), script];
    match sandbox {
        Some(sandbox) => CommandLine {
            executable: sandbox_exec,
            args: [sandbox_exec.to_owned(), "-p".into(), sandbox]
                .into_iter()
                .chain(shell)
                .collect(),
            env,
        },
        None => CommandLine {
            executable: "/bin/sh",
            args: shell.into(),
            env,
        },
    }
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
    fn sample_clock(&mut self, sample: Result<u64, ExecCmdError>) -> (u64, bool) {
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
    /// KILL is due once the profile's grace has passed since the first stop.
    fn kill_due(&self, clock_fault: bool, elapsed: u64, already_killed: bool, grace: u64) -> bool {
        self.fence_valid && !already_killed && (clock_fault || elapsed >= grace)
    }
}

/// One input read from the lease (FD 3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LeaseInput {
    /// Byte `T`.
    Term,
    /// Byte `K`.
    Kill,
    /// EOF, any other byte, or a read error: handled as EOF.
    End,
}
/// Classify one 1-byte `read` of the lease. `None` means no input yet.
fn lease_input(n: isize, byte: u8, would_block: bool) -> Option<LeaseInput> {
    match n {
        1 if byte == LEASE_TERM => Some(LeaseInput::Term),
        1 if byte == LEASE_KILL => Some(LeaseInput::Kill),
        n if n < 0 && would_block => None,
        _ => Some(LeaseInput::End),
    }
}
fn read_lease(fd: i32) -> Option<LeaseInput> {
    let mut b = [0u8; 1];
    // SAFETY: one-byte read from the validated nonblocking lease descriptor
    // into a live one-byte buffer.
    let n = unsafe { libc::read(fd, b.as_mut_ptr().cast(), 1) };
    lease_input(n, b[0], n < 0 && spawn::would_block())
}
/// Lease reads per loop pass. The service writes at most `T` and `K`; a
/// third byte is always handled as EOF, which ends lease polling.
const LEASE_READS: usize = 3;
/// The group signal one input asks for. `Kill` outranks `Term`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Signal {
    None,
    Term,
    Kill,
}
/// The guardian's stop decision and kind 2 reason
/// (tool-job-controller.md, guardian contract additions): the first stop
/// sets the reason (2 `T`, 4 EOF or fault, 0 leader exit or lost report);
/// `K` sets 11 and replaces it; a later `T` is ignored; EOF after a stop
/// only ends lease polling. Reason 12 replaces the result at settlement.
#[derive(Debug, Eq, PartialEq)]
struct StopState {
    stopped: bool,
    reason: u8,
    lease_open: bool,
}
impl StopState {
    fn new() -> Self {
        Self {
            stopped: false,
            reason: 0,
            lease_open: true,
        }
    }
    fn lease(&mut self, input: LeaseInput) -> Signal {
        match input {
            LeaseInput::Term if !self.stopped => {
                self.stopped = true;
                self.reason = REASON_CANCEL;
                Signal::Term
            }
            LeaseInput::Term => Signal::None,
            LeaseInput::Kill => {
                self.stopped = true;
                self.reason = REASON_KILL;
                Signal::Kill
            }
            LeaseInput::End => {
                self.lease_open = false;
                if self.stopped {
                    Signal::None
                } else {
                    self.stopped = true;
                    self.reason = REASON_LEASE_ENDED;
                    Signal::Term
                }
            }
        }
    }
    /// Leader exit, or a status report that can no longer be sent.
    fn unprompted(&mut self) -> Signal {
        if self.stopped {
            Signal::None
        } else {
            self.stopped = true;
            Signal::Term
        }
    }
    fn settled_reason(&self, escapees: u32) -> u8 {
        if escapees > 0 {
            REASON_ESCAPEE_TERMINATED
        } else {
            self.reason
        }
    }
}
/// Kind 5 exit facts from a `waitid` result: 1 and the exit code for
/// `CLD_EXITED`, 2 and the signal for `CLD_KILLED` or `CLD_DUMPED`.
fn leader_exit(code: i32, status: i32) -> Option<(u8, u32)> {
    match code {
        libc::CLD_EXITED if (0..=255).contains(&status) => Some((1, status as u32)),
        libc::CLD_KILLED | libc::CLD_DUMPED if (1..=127).contains(&status) => {
            Some((2, status as u32))
        }
        _ => None,
    }
}

/// Private entrypoint; inherited descriptors must pass validation before any effect.
/// The command has no time bound: it runs until it ends, its lease asks
/// for a stop or closes, or its leader exits. An unknown profile token
/// exits with status 1 before any effect and before any notice.
pub fn run_guardian(jobhex: &str, profile: &str, command: &str) -> i32 {
    match guardian(jobhex, profile, command, Faults::default()) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}
/// The guardian with one escapee-control fault (escapee-control.md,
/// test-support faults). An unknown fault refuses before any effect.
#[cfg(feature = "test-support")]
pub fn run_guardian_for_test(jobhex: &str, profile: &str, command: &str, fault: &str) -> i32 {
    let Some(fault) = escapee::Fault::parse(fault) else {
        return 1;
    };
    match guardian(jobhex, profile, command, Faults(fault)) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}
#[cfg(feature = "test-support")]
struct Faults(escapee::Fault);
#[cfg(feature = "test-support")]
impl Default for Faults {
    fn default() -> Self {
        Self(escapee::Fault::None)
    }
}
#[cfg(not(feature = "test-support"))]
#[derive(Default)]
struct Faults;
#[cfg(feature = "test-support")]
impl Faults {
    /// `group-only` reproduces the old group-only check (EC-V-OLD).
    fn group_only(&self) -> bool {
        self.0 == escapee::Fault::GroupOnly
    }
    fn apply(&self, table: &mut Libproc) {
        table.fault = self.0;
    }
    /// `no-sandbox-exec` names a path that does not exist (PC-I29).
    fn sandbox_exec(&self) -> &'static str {
        if self.0 == escapee::Fault::NoSandboxExec {
            "/usr/bin/sandbox-exec-absent-for-test"
        } else {
            SANDBOX_EXEC
        }
    }
}
#[cfg(not(feature = "test-support"))]
impl Faults {
    fn group_only(&self) -> bool {
        false
    }
    fn apply(&self, _table: &mut Libproc) {}
    fn sandbox_exec(&self) -> &'static str {
        SANDBOX_EXEC
    }
}
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
fn parse_id(s: &str) -> Result<[u8; 16], ExecCmdError> {
    if s.len() != 32
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ExecCmdError::Invalid);
    }
    let mut id = [0; 16];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| ExecCmdError::Invalid)?;
    }
    if id == [0; 16] {
        return Err(ExecCmdError::Invalid);
    }
    Ok(id)
}
/// FDs 3–10 for every profile; FD 11 is the person environment descriptor
/// for the person tokens and must be absent for `model-1`, as must the
/// root descriptors FDs 12–19.
fn descriptors(profile: Profile) -> Result<(Vec<OwnedFd>, Option<OwnedFd>), ExecCmdError> {
    for fd in 3..=10 {
        let st = stat_fd(fd).map_err(|_| ExecCmdError::Denied)?;
        // SAFETY: inspect access mode before assuming descriptor ownership.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        let access = if fd == 3 || fd >= 7 {
            libc::O_RDONLY
        } else {
            libc::O_WRONLY
        };
        if flags < 0 || flags & libc::O_ACCMODE != access || st.st_uid != unsafe { libc::geteuid() }
        {
            return Err(ExecCmdError::Denied);
        }
        if fd <= 6 {
            if st.st_mode & libc::S_IFMT != libc::S_IFIFO {
                return Err(ExecCmdError::Denied);
            }
        } else if st.st_mode & libc::S_IFMT != libc::S_IFDIR || st.st_nlink == 0 {
            return Err(ExecCmdError::Denied);
        }
        if (fd == 3 || fd == 4) && flags & libc::O_NONBLOCK == 0 {
            return Err(ExecCmdError::Denied);
        }
        if fd >= 9 && st.st_mode & 0o777 != 0o700 {
            return Err(ExecCmdError::Denied);
        }
    }
    let frame = if profile.person() {
        let fd = person::FRAME_FD;
        let st = stat_fd(fd).map_err(|_| ExecCmdError::Denied)?;
        // SAFETY: inspect access mode before assuming descriptor ownership.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0
            || flags & libc::O_ACCMODE != libc::O_RDONLY
            || st.st_mode & libc::S_IFMT != libc::S_IFIFO
            || st.st_uid != unsafe { libc::geteuid() }
        {
            return Err(ExecCmdError::Denied);
        }
        // SAFETY: this private invocation exclusively receives FD 11.
        Some(unsafe { OwnedFd::from_raw_fd(fd) })
    } else {
        person::absent(person::FRAME_FD..=person::FIRST_ROOT_FD + person::MAX_ROOTS as i32 - 1)?;
        None
    };
    // SAFETY: this private invocation exclusively receives the validated FDs.
    Ok((
        (3..=10)
            .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
            .collect(),
        frame,
    ))
}
pub(super) fn pinned_path(fd: i32) -> Result<String, ExecCmdError> {
    let path = spawn::path(fd)?;
    let (device, inode) =
        crate::project_identity::pinned_identity(&path).map_err(|_| ExecCmdError::Denied)?;
    let held = stat_fd(fd).map_err(|_| ExecCmdError::Denied)?;
    if device != held.st_dev as u64 || inode != held.st_ino {
        return Err(ExecCmdError::Denied);
    }
    Ok(path)
}
/// `path` is `root` or lies beneath it.
pub(super) fn within(path: &str, root: &str) -> bool {
    path == root || path.starts_with(&format!("{}/", root.trim_end_matches('/')))
}
/// A path check inside the guardian. Storage's identity check is the authority.
fn separate_from_state(project: &str, scratch: &str, asura: &str) -> Result<(), ExecCmdError> {
    if !scratch.starts_with(&format!("{asura}/tmp/"))
        || within(project, asura)
        || within(asura, project)
    {
        return Err(ExecCmdError::Denied);
    }
    Ok(())
}
/// A Seatbelt string literal, from the existing quoting helper.
pub(super) fn quote(s: &str) -> Result<String, ExecCmdError> {
    crate::git_observer::quoted(s).map_err(|_| ExecCmdError::Denied)
}
/// ` (literal P)` for every proper ancestor of every root, so path lookups
/// can stat their way down to each root.
pub(super) fn ancestor_literals(roots: &[&str]) -> Result<String, ExecCmdError> {
    let mut ancestors = std::collections::BTreeSet::new();
    for root in roots {
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
    Ok(metadata)
}
/// The last matching rule wins: deny the Asura home, then re-allow scratch.
pub(super) fn state_rules(asura: &str, scratch: &str) -> Result<String, ExecCmdError> {
    Ok(format!(
        "(deny file-read* file-write* process-exec file-map-executable (subpath {asura}))(allow process-exec file-map-executable file-read* (subpath {scratch}))(allow file-write* (subpath {scratch}))(allow file-read-metadata (literal {asura}) (literal {tmp}))",
        asura = quote(asura)?,
        scratch = quote(scratch)?,
        tmp = quote(&format!("{asura}/tmp"))?,
    ))
}
/// The implicit-execution hook directory (PX1 list, execution-path-guard.md
/// "Path lists"): the directory and every file under it.
const IMPLICIT_HOOKS: &str = ".git/hooks";
/// The implicit-execution files of the same list.
const IMPLICIT_FILES: [&str; 6] = [
    ".git/config",
    ".gitattributes",
    ".gitmodules",
    ".envrc",
    ".cargo/config",
    ".cargo/config.toml",
];
/// Directories between the project and a nested implicit-execution path:
/// 1 to 8 (execution-path-guard.md "Path lists").
const IMPLICIT_NESTED_DEPTH: usize = 8;
/// The `model-1` deny rules for the implicit-execution paths of `project`
/// (EPG-R1). They follow the write allow rules, so they win. Root forms use
/// `subpath` and `literal` filters; nested forms, 1 to 8 directories below
/// the project, use one `regex` filter in which the project path and the
/// listed names are escaped by [`crate::git_observer::regex_escaped`].
/// Seatbelt's regex dialect does not honour `{m,n}` (such a filter never
/// matches; macOS 27 probe, 2026-10-23), so the depth bound is one required
/// directory group followed by seven nested optional groups.
fn implicit_deny_rules(project: &str) -> Result<String, ExecCmdError> {
    let escape = |s: &str| crate::git_observer::regex_escaped(s).map_err(|_| ExecCmdError::Denied);
    let project = project.trim_end_matches('/');
    let mut filters = format!(
        " (subpath {})",
        quote(&format!("{project}/{IMPLICIT_HOOKS}"))?
    );
    let mut forms = vec![format!("{}(/.*)?", escape(IMPLICIT_HOOKS)?)];
    for file in IMPLICIT_FILES {
        filters.push_str(&format!(
            " (literal {})",
            quote(&format!("{project}/{file}"))?
        ));
        forms.push(escape(file)?);
    }
    let directory = "[^/]+/";
    let nested = format!(
        "{directory}{}{}",
        format!("({directory}").repeat(IMPLICIT_NESTED_DEPTH - 1),
        ")?".repeat(IMPLICIT_NESTED_DEPTH - 1)
    );
    let regex = format!("^{}/{nested}({})$", escape(project)?, forms.join("|"));
    filters.push_str(&format!(" (regex {})", quote(&regex)?));
    Ok(format!("(deny file-write*{filters})"))
}
fn seatbelt(project: &str, scratch: &str, asura: &str) -> Result<String, ExecCmdError> {
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
    for root in roots {
        filters.push_str(&format!(" (subpath {})", quote(root)?));
    }
    let metadata = ancestor_literals(&roots)?;
    let state = state_rules(asura, scratch)?;
    let deny = implicit_deny_rules(project)?;
    Ok(format!(
        "(version 1)(deny default)(allow file-read* file-test-existence (literal \"/\"))(allow file-read* (literal \"/private/var/select/sh\"))(allow file-read-metadata (literal \"/private\") (literal \"/private/var\") (literal \"/private/var/select\"))(allow process-fork)(allow process-exec file-map-executable file-read*{filters})(allow file-read-metadata{metadata})(allow file-write* (subpath {}) (subpath {}))(allow sysctl-read)(allow file-read* file-write* (literal \"/dev/null\"))(allow file-read* (literal \"/dev/random\") (literal \"/dev/urandom\")){state}{deny}",
        quote(project)?,
        quote(scratch)?
    ))
}
fn notice(fd: i32, n: Notice, id: [u8; 16]) -> Result<(), ExecCmdError> {
    let bytes = n.encode(id);
    // SAFETY: bounded atomic write (<PIPE_BUF) to private nonblocking status pipe.
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if written == 32 {
        Ok(())
    } else {
        Err(ExecCmdError::Protocol)
    }
}
fn guardian(
    jobhex: &str,
    profile: &str,
    command: &str,
    faults: Faults,
) -> Result<(), ExecCmdError> {
    let id = parse_id(jobhex)?;
    let profile = Profile::parse(profile).ok_or(ExecCmdError::Invalid)?;
    if command.is_empty() || command.len() > MAX_COMMAND_BYTES || command.contains('\0') {
        return Err(ExecCmdError::Invalid);
    }
    let first_sample = spawn::monotonic_ns()?;
    let (descriptors, mut frame_fd) = descriptors(profile)?;
    // SAFETY: guardian is a single-purpose fresh child, before spawning command.
    unsafe {
        libc::umask(0o077);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let setup = (|| {
        // FD 11 before any other setup step; it is closed once read.
        let frame = match frame_fd.take() {
            Some(fd) => {
                let bytes = person::read_frame(fd.as_raw_fd(), 3, &mut || read_lease(3).is_some())?;
                drop(fd);
                Some(person::parse_frame(&bytes, profile)?)
            }
            None => None,
        };
        let secrets = own_secret_environment()?;
        // The whole-command variable runs only the fixed `eval` text.
        if secrets.iter().any(|s| s.name == SecretName::Command) && command != WHOLE_COMMAND {
            return Err(ExecCmdError::Invalid);
        }
        let project = pinned_path(7)?;
        let cwd = pinned_path(8)?;
        let scratch = pinned_path(9)?;
        let asura = pinned_path(10)?;
        if cwd != project && !cwd.starts_with(&format!("{project}/")) {
            return Err(ExecCmdError::Denied);
        }
        separate_from_state(&project, &scratch, &asura)?;
        let roots = match &frame {
            Some(frame) => person::root_paths(&frame.roles)?,
            None => Vec::new(),
        };
        let sandbox = match profile {
            Profile::Model1 => Some(seatbelt(&project, &scratch, &asura)?),
            Profile::Person1 => Some(person::seatbelt(&person::PersonPaths::from_roots(
                &roots, &project, &scratch, &asura,
            )?)?),
            Profile::PersonOutside1 => None,
        };
        // A sandboxed profile never runs without `sandbox-exec`; only the
        // outside profile does not need it (PC-I29).
        let sandbox_exec = faults.sandbox_exec();
        if sandbox.is_some() && !executable(sandbox_exec) {
            return Err(ExecCmdError::Unavailable);
        }
        // EX5: marker, decoy, PID buffer and capture pipe handles exist
        // before the command does; a failure refuses the job. A `!!` job
        // attributes by its owned group and its environment marker.
        let marker = job_marker(jobhex);
        let mut table = Libproc::new(&scratch, &asura, escapee::capture_handles([5, 6])?)?;
        faults.apply(&mut table);
        let env_marker = match profile {
            Profile::PersonOutside1 => Some(EnvMarker::new(&marker)?),
            Profile::Model1 | Profile::Person1 => None,
        };
        // Any lease input before the command exists refuses the job.
        if read_lease(3).is_some() {
            return Err(ExecCmdError::Cancelled);
        }
        let null = open_at(libc::AT_FDCWD, c"/dev/null", libc::O_RDONLY, 0)
            .map_err(|_| ExecCmdError::Io)?;
        // The preamble is added here, after both 8192-byte command checks;
        // the script is bounded by the command limit plus PREAMBLE_LIMIT.
        let script = format!(
            "{}{command}",
            preamble(secrets.iter().map(|secret| secret.name))
        );
        if script.len() > MAX_COMMAND_BYTES + PREAMBLE_LIMIT {
            return Err(ExecCmdError::Invalid);
        }
        let line = command_line(
            profile,
            sandbox_exec,
            sandbox,
            script,
            &scratch,
            &marker,
            frame
                .as_ref()
                .map_or(&[][..], |frame| frame.entries.as_slice()),
            &secrets,
        );
        let env: Vec<&[u8]> = line.env.iter().map(|entry| entry.as_slice()).collect();
        spawn::launch(
            Path::new(line.executable),
            &line.args,
            &env,
            &[(null.as_raw_fd(), 0), (5, 1), (6, 2)],
            Some(8),
            true,
        )
        .map(|pid| (pid, table, env_marker))
    })();
    let (pid, mut table, mut env_marker) = match setup {
        Ok(spawned) => spawned,
        Err(error) => {
            notice(
                4,
                Notice {
                    kind: 3,
                    reason: match error {
                        ExecCmdError::Denied => 7,
                        ExecCmdError::Unavailable => 6,
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
    let mut stop = StopState::new();
    // Clock sample of the first stop; the grace and the 2.5 s report
    // point count from it.
    let mut stopping: Option<u64> = None;
    let mut killed = false;
    // Kind 5 facts: kind 2 repeats them.
    let mut leader: Option<(u8, u32)> = None;
    let mut reported_unknown = false;
    let mut safety = CleanupSafety::new(first_sample);
    // SAFETY: getpid has no preconditions.
    let exclude = [unsafe { libc::getpid() }, pid];
    let mut tally = Tally::new(table.limit());
    if let Some(env) = env_marker.as_mut() {
        env.set_group(pid);
    }
    let outside = profile == Profile::PersonOutside1;
    let mut last_sweep: Option<Result<Sweep, escapee::Unverifiable>> = None;
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
        // Kind 5 once, when the leader's exit is first observed: before the
        // group stop signal and before any escapee sweep.
        if exited && leader.is_none() {
            // SAFETY: waitid filled `info` for this exited child.
            leader = leader_exit(info.si_code, unsafe { info.si_status() });
            match leader {
                Some((exit_kind, value)) if report_ok => {
                    report_ok = notice(
                        4,
                        Notice {
                            kind: KIND_LEADER_EXITED,
                            reason: 0,
                            exit_kind,
                            cleanup: false,
                            value,
                        },
                        id,
                    )
                    .is_ok();
                }
                Some(_) => {}
                // Unknown exit facts cannot be reported consistently.
                None => report_ok = false,
            }
        }
        // Lease input, then leader exit: `T`, `K` and EOF set the reason
        // first; reason 1 is unassigned.
        let mut signal = Signal::None;
        for _ in 0..LEASE_READS {
            if !stop.lease_open {
                break;
            }
            match read_lease(3) {
                Some(input) => signal = signal.max(stop.lease(input)),
                None => break,
            }
        }
        // PQ3: a `!!` shell exit with no stop signals nothing; the leader
        // is reaped and its survivors are counted at once.
        let shell_exit = outside && exited && !stop.stopped && report_ok;
        if (exited && !shell_exit) || !report_ok {
            signal = signal.max(stop.unprompted());
        }
        if stop.stopped && stopping.is_none() {
            stopping = Some(now);
        }
        match signal {
            Signal::Term => {
                // SAFETY: leader has never been reaped; its PID fences this owned group.
                if safety.fence_valid {
                    unsafe {
                        libc::kill(-pid, libc::SIGTERM);
                    }
                }
            }
            Signal::Kill => {
                // SAFETY: as above; `K` kills the owned group now.
                if safety.fence_valid && !killed {
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                }
                killed = true;
            }
            Signal::None => {}
        }
        let mut ready = shell_exit && safety.fence_valid;
        if let Some(start) = stopping {
            if safety.kill_due(
                clock_fault,
                now.saturating_sub(start),
                killed,
                profile.grace_ns(),
            ) {
                if safety.fence_valid {
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                }
                killed = true;
            }
            if safety.fence_valid && exited && group_only_leader(pid).unwrap_or(false) {
                ready = true;
            }
        }
        if ready {
            // EX5 settlement gate: the leader stays unreaped, as the group
            // fence, until no attributed descendant remains. The `!!` gate
            // needs only the leader: its survivors are counted after the
            // reap and never signalled (EC-D1, OD2).
            let gate = if outside {
                Gate::Settled
            } else {
                let swept = if faults.group_only() {
                    Ok(Sweep::Clear)
                } else {
                    escapee::sweep(&mut table, &exclude, &mut tally, FREEZE_ROUND_LIMIT)
                };
                last_sweep = Some(swept);
                escapee::gate(GateFacts {
                    leader_exited: true,
                    group_empty: true,
                    sweep: swept.ok(),
                })
            };
            match gate {
                Gate::Settled => {
                    let mut status = 0;
                    // SAFETY: our zombie leader is reaped only after all
                    // group and escapee signals end.
                    let reaped = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                    if reaped == pid {
                        let (escapees, reason) = if let Some(env) = env_marker.as_mut() {
                            let found = escapee::survivors(&mut table, env, &exclude);
                            (found.count, found.settled_reason(stop.reason))
                        } else {
                            let escapees = tally.count();
                            (escapees, stop.settled_reason(escapees))
                        };
                        // Kind 2 repeats the kind 5 exit facts.
                        if let (true, Some((exit_kind, value))) = (report_ok, leader) {
                            if escapees > 0 {
                                notice(
                                    4,
                                    Notice {
                                        kind: KIND_ESCAPEE_REPORT,
                                        reason: 0,
                                        exit_kind: 0,
                                        cleanup: false,
                                        value: escapees,
                                    },
                                    id,
                                )?;
                            }
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
                            Err(ExecCmdError::Protocol)
                        };
                    }
                }
                // EC-D2: an unverifiable scan is reported at once.
                Gate::Unverifiable if !reported_unknown => {
                    if report_ok {
                        report_ok = notice(4, unconfirmed(REASON_ESCAPEE_UNTERMINATED), id).is_ok();
                    }
                    reported_unknown = true;
                }
                Gate::Unverifiable | Gate::Wait => {}
            }
        }
        if let Some(start) = stopping
            && now.saturating_sub(start) >= 2_500_000_000
            && !reported_unknown
        {
            let reason = match last_sweep {
                Some(Ok(Sweep::Remaining(_)) | Err(_)) => REASON_ESCAPEE_UNTERMINATED,
                _ => REASON_CLEANUP_FAILED,
            };
            if report_ok {
                report_ok = notice(4, unconfirmed(reason), id).is_ok();
            }
            reported_unknown = true;
        }
        // One paced polling wait of at most 100 ms. FD 3 is watched until
        // its EOF, so `K` after a stop wakes the loop and a hung-up lease
        // never spins it.
        let mut pfd = libc::pollfd {
            fd: if stop.lease_open { 3 } else { -1 },
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe {
            libc::poll(&mut pfd, 1, 100);
        }
    }
}
/// Kind 4: cleanup is not confirmed; the paced sweeps continue.
fn unconfirmed(reason: u8) -> Notice {
    Notice {
        kind: 4,
        reason,
        exit_kind: 0,
        cleanup: false,
        value: 0,
    }
}
fn group_only_leader(pid: libc::pid_t) -> Result<bool, ExecCmdError> {
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
        return Err(ExecCmdError::CleanupUnconfirmed);
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
        let text = seatbelt("/p", "/h/.asura/tmp/exec-cmd-1", "/h/.asura").unwrap();
        let deny = text
            .find("(deny file-read* file-write* process-exec file-map-executable (subpath \"/h/.asura\"))")
            .expect("state deny");
        let exec = text
            .rfind("(allow process-exec file-map-executable file-read* (subpath \"/h/.asura/tmp/exec-cmd-1\"))")
            .expect("scratch exec");
        let write = text
            .rfind("(allow file-write* (subpath \"/h/.asura/tmp/exec-cmd-1\"))")
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
        let quoted = seatbelt("/p", "/h/.as\"ura/tmp/s", "/h/.as\"ura").unwrap();
        assert!(quoted.contains("(subpath \"/h/.as\\\"ura\")"));
    }
    /// EPG-U5: the `model-1` profile ends with the implicit-execution deny
    /// rules for a project path holding every regex metacharacter and a
    /// Seatbelt string quote; the path is escaped in the regex and quoted in
    /// every filter, and the rules follow the project write allow.
    #[test]
    fn epg_u5_model_profile_denies_implicit_paths_with_escaped_project() {
        let project = r#"/w/p.a+b(c)[d]{e}$f^g|h*i?j\k"l"#;
        let text = seatbelt(project, "/h/.asura/tmp/exec-cmd-1", "/h/.asura").unwrap();
        let q = r#"/w/p.a+b(c)[d]{e}$f^g|h*i?j\\k\"l"#;
        let rx = r#"/w/p\\.a\\+b\\(c\\)\\[d\\]\\{e\\}\\$f\\^g\\|h\\*i\\?j\\\\k\"l"#;
        let dir = "[^/]+/";
        let nested = format!("{dir}{}{}", format!("({dir}").repeat(7), ")?".repeat(7));
        let expected = format!(
            concat!(
                "(deny file-write* (subpath \"{q}/.git/hooks\")",
                " (literal \"{q}/.git/config\") (literal \"{q}/.gitattributes\")",
                " (literal \"{q}/.gitmodules\") (literal \"{q}/.envrc\")",
                " (literal \"{q}/.cargo/config\") (literal \"{q}/.cargo/config.toml\")",
                " (regex \"^{rx}/{nested}(\\\\.git/hooks(/.*)?|\\\\.git/config|\\\\.gitattributes",
                "|\\\\.gitmodules|\\\\.envrc|\\\\.cargo/config|\\\\.cargo/config\\\\.toml)$\"))"
            ),
            q = q,
            rx = rx,
            nested = nested
        );
        assert!(text.ends_with(&expected), "{text}");
        assert_eq!(text.matches("(deny file-write*").count(), 1);
        let allow = text
            .find(&format!("(allow file-write* (subpath \"{q}\")"))
            .expect("project write allow");
        let state = text
            .find("(deny file-read* file-write*")
            .expect("state rules");
        assert!(allow < state && state < text.len() - expected.len());
        // A trailing slash on the project does not double the separator.
        let trailing = implicit_deny_rules("/w/p/").unwrap();
        assert!(trailing.starts_with("(deny file-write* (subpath \"/w/p/.git/hooks\")"));
        assert!(trailing.contains("(regex \"^/w/p/[^/]+/"));
        // A control character in the project refuses the profile.
        assert_eq!(
            seatbelt("/w/p\nq", "/h/.asura/tmp/s", "/h/.asura"),
            Err(ExecCmdError::Denied)
        );
    }
    fn numbers(entries: &[&[u8]]) -> Result<Vec<u8>, ExecCmdError> {
        secret_environment(entries.iter().copied()).map(|accepted| {
            accepted
                .iter()
                .map(|s| match s.name {
                    SecretName::Numbered(n) => n,
                    SecretName::Command => 0,
                })
                .collect()
        })
    }
    /// SDU-17 (guardian half): `ASURA_SECRET_COMMAND` is accepted alone with
    /// 1–8192 bytes; beside any numbered variable, twice, empty, with a NUL
    /// or over 8192 bytes it refuses the set.
    #[test]
    fn sdu_17_whole_command_variable_only_alone_and_bounded() {
        let entry = |value: &[u8]| {
            let mut e = format!("{SECRET_COMMAND_NAME}=").into_bytes();
            e.extend_from_slice(value);
            e
        };
        let one = entry(b"echo 'x' | tr x y");
        let max = entry(&[b'v'; 8192]);
        let over = entry(&[b'v'; 8193]);
        let empty = entry(b"");
        let nul = entry(b"a\0b");
        assert_eq!(numbers(&[&one]), Ok(vec![0]));
        assert_eq!(
            numbers(&[&b"__CF_USER_TEXT_ENCODING=0x1F5:0x0:0x0"[..], &max]),
            Ok(vec![0]),
            "8192 bytes"
        );
        for (label, entries) in [
            ("8193 bytes", vec![over.as_slice()]),
            ("empty", vec![empty.as_slice()]),
            ("NUL", vec![nul.as_slice()]),
            ("twice", vec![one.as_slice(), one.as_slice()]),
            ("numbered after", vec![one.as_slice(), b"ASURA_SECRET_1=a"]),
            (
                "numbered before",
                vec![&b"ASURA_SECRET_16=a"[..], one.as_slice()],
            ),
            (
                "numbered empty",
                vec![&b"ASURA_SECRET_2="[..], one.as_slice()],
            ),
            ("lower case", vec![&b"asura_secret_command=a"[..]]),
            ("suffix", vec![&b"ASURA_SECRET_COMMANDS=a"[..]]),
        ] {
            assert_eq!(numbers(&entries), Err(ExecCmdError::Invalid), "{label}");
        }
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
            assert_eq!(numbers(&entries), Err(ExecCmdError::Invalid), "{label}");
        }
        let mut seventeen = all.clone();
        seventeen.push(b"ASURA_SECRET_1=a");
        assert_eq!(
            numbers(&seventeen),
            Err(ExecCmdError::Invalid),
            "17 entries"
        );
    }
    #[test]
    fn preamble_holds_names_only_and_is_bounded() {
        use SecretName::{Command, Numbered};
        assert_eq!(preamble([]), "");
        assert_eq!(
            preamble([Command]),
            "__asura_t=$ASURA_SECRET_COMMAND; unset ASURA_SECRET_COMMAND; \
             ASURA_SECRET_COMMAND=$__asura_t; unset __asura_t; "
        );
        assert!(preamble([Command]).len() <= PREAMBLE_LIMIT);
        assert_eq!(
            preamble([Numbered(1), Numbered(12)]),
            "__asura_t=$ASURA_SECRET_1; unset ASURA_SECRET_1; ASURA_SECRET_1=$__asura_t; \
             __asura_t=$ASURA_SECRET_12; unset ASURA_SECRET_12; ASURA_SECRET_12=$__asura_t; \
             unset __asura_t; "
        );
        let longest = preamble((1..=16).map(Numbered));
        assert_eq!(longest.len(), 1254);
        assert!(longest.len() <= PREAMBLE_LIMIT);
        assert!(!longest.contains('\n'), "the command keeps its first line");
    }
    #[test]
    fn guardian_path_check_rejects_every_overlap() {
        let asura = "/h/.asura";
        let scratch = "/h/.asura/tmp/exec-cmd-1";
        assert!(separate_from_state("/h/project", scratch, asura).is_ok());
        assert!(separate_from_state("/h/.asura-other", scratch, asura).is_ok());
        for project in ["/h/.asura", "/h/.asura/run", "/h", "/"] {
            assert_eq!(
                separate_from_state(project, scratch, asura),
                Err(ExecCmdError::Denied),
                "{project}"
            );
        }
        assert_eq!(
            separate_from_state("/h/project", "/h/project/tmp/x", asura),
            Err(ExecCmdError::Denied)
        );
        assert_eq!(
            separate_from_state("/h/project", "/h/.asura/tmpx/x", asura),
            Err(ExecCmdError::Denied)
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
        let scratch = asura.join("tmp/exec-cmd-1");
        std::fs::create_dir_all(&scratch).unwrap();
        for dir in [&home, &asura, &scratch] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::write(asura.join("secret"), b"state").unwrap();
        std::fs::write(home.join("visible"), b"project").unwrap();
        let text = seatbelt(
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
            safety.sample_clock(Err(ExecCmdError::Io)),
            (5_200_000_000, true)
        );
        assert_eq!(
            safety.sample_clock(Err(ExecCmdError::Io)),
            (5_300_000_000, true)
        );
        let mut fresh = CleanupSafety::new(7);
        assert_eq!(
            fresh.sample_clock(Err(ExecCmdError::Io)),
            (100_000_007, true)
        );
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
        assert!(!safety.kill_due(true, u64::MAX, false, 500_000_000));
    }
    #[test]
    fn repeated_clock_failure_advances_cleanup_and_kills_only_owned_group() {
        let mut safety = CleanupSafety::new(1_000);
        let (first, fault) = safety.sample_clock(Err(ExecCmdError::Io));
        assert!(first >= 1_000);
        assert!(fault);
        let grace = Profile::Model1.grace_ns();
        assert!(safety.kill_due(fault, 0, false, grace));
        let (second, fault) = safety.sample_clock(Err(ExecCmdError::Io));
        assert!(second > first);
        assert!(fault);
        assert!(!safety.kill_due(fault, 0, true, grace));
        assert!(!safety.kill_due(false, 499_999_999, false, grace));
        assert!(safety.kill_due(false, 500_000_000, false, grace));
        assert!(!safety.kill_due(false, 1_999_999_999, false, 2_000_000_000));
        assert!(safety.kill_due(false, 2_000_000_000, false, 2_000_000_000));
        safety.observe_wait(-1, Some(libc::EINVAL));
        assert!(!safety.kill_due(fault, second - first, false, grace));
    }
    /// J2T-05 unit: lease reads. `T` and `K` are stop bytes; EOF, any
    /// other byte and a read error are handled as EOF; no data is no input.
    #[test]
    fn j2t_05_lease_input_classification() {
        assert_eq!(lease_input(1, b'T', false), Some(LeaseInput::Term));
        assert_eq!(lease_input(1, b'K', false), Some(LeaseInput::Kill));
        for byte in [b't', b'k', 0, b'x', 0xff] {
            assert_eq!(lease_input(1, byte, false), Some(LeaseInput::End), "{byte}");
        }
        assert_eq!(lease_input(0, 0, false), Some(LeaseInput::End), "EOF");
        assert_eq!(lease_input(-1, 0, true), None, "would block");
        assert_eq!(lease_input(-1, 0, false), Some(LeaseInput::End), "error");
    }
    /// J2T-05 unit: `T`, a second `T`, `K`, EOF after `T`, an unknown byte,
    /// and the reason precedence 2, 11, 4, 12.
    #[test]
    fn j2t_05_lease_bytes_and_reason_precedence() {
        use LeaseInput::{End, Kill, Term};
        // T: first stop with TERM, reason 2; a second T is ignored.
        let mut s = StopState::new();
        assert_eq!(s.lease(Term), Signal::Term);
        assert_eq!(s.reason, REASON_CANCEL);
        assert_eq!(s.lease(Term), Signal::None);
        assert_eq!(s.reason, REASON_CANCEL);
        // K escalates T: KILL now, reason 11 replaces 2.
        assert_eq!(s.lease(Kill), Signal::Kill);
        assert_eq!(s.reason, REASON_KILL);
        assert_eq!(s.lease(Term), Signal::None, "T after K is ignored");
        // EOF after the stop only ends lease polling.
        assert!(s.lease_open);
        assert_eq!(s.lease(End), Signal::None);
        assert!(!s.lease_open);
        assert_eq!(s.reason, REASON_KILL);
        // Reason 12 replaces the lease reason only with escapees.
        assert_eq!(s.settled_reason(0), REASON_KILL);
        assert_eq!(s.settled_reason(3), REASON_ESCAPEE_TERMINATED);

        // EOF after T keeps reason 2.
        let mut s = StopState::new();
        s.lease(Term);
        assert_eq!(s.lease(End), Signal::None);
        assert_eq!((s.reason, s.lease_open), (REASON_CANCEL, false));
        assert_eq!(s.unprompted(), Signal::None, "leader exit after a stop");
        assert_eq!(s.settled_reason(0), REASON_CANCEL);

        // K first: KILL now, reason 11.
        let mut s = StopState::new();
        assert_eq!(s.lease(Kill), Signal::Kill);
        assert_eq!((s.stopped, s.reason), (true, REASON_KILL));

        // EOF or an unknown byte (End) first: TERM, reason 4, polling ends.
        let mut s = StopState::new();
        assert_eq!(s.lease(End), Signal::Term);
        assert_eq!((s.reason, s.lease_open), (REASON_LEASE_ENDED, false));
        assert_eq!(s.settled_reason(0), REASON_LEASE_ENDED);
        assert_eq!(s.settled_reason(1), REASON_ESCAPEE_TERMINATED);

        // Leader exit with no lease input: TERM, reason 0; K escalates it.
        let mut s = StopState::new();
        assert_eq!(s.unprompted(), Signal::Term);
        assert_eq!(s.reason, 0);
        assert!(s.lease_open, "lease polling continues after the stop");
        assert_eq!(s.lease(Term), Signal::None);
        assert_eq!(s.reason, 0, "T after a leader-exit stop is ignored");
        assert_eq!(s.lease(Kill), Signal::Kill);
        assert_eq!(s.reason, REASON_KILL);

        // In one pass, K outranks T.
        assert_eq!(Signal::Term.max(Signal::Kill), Signal::Kill);
        assert_eq!(Signal::None.max(Signal::Term), Signal::Term);
    }
    /// PC-U24: the person profiles' lease bytes. `T` starts TERM and the
    /// 2 s grace (PC8), `K` kills at once, EOF ends the lease and stops the
    /// group, and an unknown byte is handled as EOF (the J2 lease rule: a
    /// lease fault). The grace is the profile's, not the model's 500 ms.
    #[test]
    fn pc_u24_person_lease_bytes_and_grace() {
        for profile in [Profile::Person1, Profile::PersonOutside1] {
            let grace = profile.grace_ns();
            assert_eq!(grace, 2_000_000_000, "{profile:?}");
            let safety = CleanupSafety::new(0);
            // T: TERM now, KILL only once the 2 s grace has passed.
            let mut s = StopState::new();
            assert_eq!(s.lease(lease_input(1, b'T', false).unwrap()), Signal::Term);
            assert_eq!(s.reason, REASON_CANCEL);
            assert!(!safety.kill_due(false, 500_000_000, false, grace));
            assert!(!safety.kill_due(false, 1_999_999_999, false, grace));
            assert!(safety.kill_due(false, 2_000_000_000, false, grace));
            // K: KILL now, reason 11.
            assert_eq!(s.lease(lease_input(1, b'K', false).unwrap()), Signal::Kill);
            assert_eq!(s.reason, REASON_KILL);
            // EOF: lease end, TERM, reason 4.
            let mut s = StopState::new();
            assert_eq!(s.lease(lease_input(0, 0, false).unwrap()), Signal::Term);
            assert_eq!((s.reason, s.lease_open), (REASON_LEASE_ENDED, false));
            // An unknown byte is a lease fault, handled as EOF.
            let mut s = StopState::new();
            assert_eq!(lease_input(1, b'x', false), Some(LeaseInput::End));
            assert_eq!(s.lease(LeaseInput::End), Signal::Term);
            assert_eq!(s.reason, REASON_LEASE_ENDED);
        }
    }
    fn secret(entry: &[u8], number: u8) -> SecretEntry {
        SecretEntry {
            name: SecretName::Numbered(number),
            entry: Zeroizing::new(entry.to_vec()),
        }
    }
    /// PC-U46: `PersonOutside` runs `/bin/sh -c COMMAND` with no
    /// `sandbox-exec` and no profile; envp is the FD 11 launch environment,
    /// then `ASURA_JOB_<jobhex>=1`, then the secret variables, and the
    /// preamble is present when secret variables exist. The guardian's own
    /// environment check refuses a launch entry, so none can reach it.
    #[test]
    fn pc_u46_outside_command_line() {
        let jobhex = "00112233445566778899aabbccddeeff";
        let marker = job_marker(jobhex);
        assert_eq!(marker, format!("ASURA_JOB_{jobhex}=1"));
        let frame = [
            Zeroizing::new(b"PATH=/opt/x/bin:/usr/bin".to_vec()),
            Zeroizing::new(b"DYLD_INSERT_LIBRARIES=/tmp/x.dylib".to_vec()),
            Zeroizing::new(b"HOME=/Users/p".to_vec()),
        ];
        let secrets = [secret(b"ASURA_SECRET_1=hunter2", 1)];
        let script = format!(
            "{}echo \"$ASURA_SECRET_1\"",
            preamble(secrets.iter().map(|s| s.name))
        );
        let line = command_line(
            Profile::PersonOutside1,
            SANDBOX_EXEC,
            None,
            script.clone(),
            "/h/.asura/tmp/exec-cmd-1",
            &marker,
            &frame,
            &secrets,
        );
        assert_eq!(line.executable, "/bin/sh");
        assert_eq!(
            line.args,
            vec!["/bin/sh".to_owned(), "-c".into(), script.clone()]
        );
        assert!(
            line.args
                .iter()
                .all(|a| !a.contains("sandbox-exec") && !a.contains("(version 1)"))
        );
        assert!(script.starts_with("__asura_t=$ASURA_SECRET_1; unset ASURA_SECRET_1;"));
        let env: Vec<&[u8]> = line.env.iter().map(|e| e.as_slice()).collect();
        assert_eq!(
            env,
            vec![
                &b"PATH=/opt/x/bin:/usr/bin"[..],
                b"DYLD_INSERT_LIBRARIES=/tmp/x.dylib",
                b"HOME=/Users/p",
                marker.as_bytes(),
                b"ASURA_SECRET_1=hunter2",
            ]
        );
        // Without secrets: no preamble, the marker is last.
        let line = command_line(
            Profile::PersonOutside1,
            SANDBOX_EXEC,
            None,
            format!("{}true", preamble([])),
            "/s",
            &marker,
            &frame,
            &[],
        );
        assert_eq!(line.args[2], "true");
        assert_eq!(line.env.last().unwrap().as_slice(), marker.as_bytes());
        // The guardian never adopts a launch entry: its own environment
        // check refuses every non-secret name.
        for entry in frame.iter() {
            assert!(secret_environment([entry.as_slice()]).is_err());
        }
        // `person-1` keeps `sandbox-exec`, its FD 11 entries and no marker;
        // `model-1` keeps its fixed environment.
        let line = command_line(
            Profile::Person1,
            SANDBOX_EXEC,
            Some("(version 1)".into()),
            "true".into(),
            "/s",
            &marker,
            &frame,
            &[],
        );
        assert_eq!(line.executable, SANDBOX_EXEC);
        assert_eq!(&line.args[..3], &[SANDBOX_EXEC, "-p", "(version 1)"]);
        assert_eq!(&line.args[3..], &["/bin/sh", "-c", "true"]);
        assert_eq!(line.env.len(), 3);
        assert!(line.env.iter().all(|e| !e.starts_with(b"ASURA_JOB_")));
        let line = command_line(
            Profile::Model1,
            SANDBOX_EXEC,
            Some("(version 1)".into()),
            "true".into(),
            "/s",
            &marker,
            &frame,
            &[],
        );
        let env: Vec<&[u8]> = line.env.iter().map(|e| e.as_slice()).collect();
        assert_eq!(
            env,
            vec![
                &b"PATH=/usr/bin:/bin:/usr/sbin:/sbin"[..],
                b"LC_ALL=C",
                b"LANG=C",
                b"HOME=/s",
                b"TMPDIR=/s",
            ]
        );
    }
    /// J2T-06 unit: kind 5 facts from `waitid`.
    #[test]
    fn j2t_06_leader_exit_facts() {
        assert_eq!(leader_exit(libc::CLD_EXITED, 0), Some((1, 0)));
        assert_eq!(leader_exit(libc::CLD_EXITED, 255), Some((1, 255)));
        assert_eq!(leader_exit(libc::CLD_KILLED, 9), Some((2, 9)));
        assert_eq!(leader_exit(libc::CLD_DUMPED, 6), Some((2, 6)));
        assert_eq!(leader_exit(libc::CLD_KILLED, 127), Some((2, 127)));
        for (code, status) in [
            (libc::CLD_EXITED, 256),
            (libc::CLD_EXITED, -1),
            (libc::CLD_KILLED, 0),
            (libc::CLD_KILLED, 128),
            (libc::CLD_STOPPED, 17),
            (0, 0),
        ] {
            assert_eq!(leader_exit(code, status), None, "{code} {status}");
        }
    }
    /// J2T-25 unit (J2-A2): pins the order the service's pre-spawn token
    /// release rests on. Every return before `spawn::launch` succeeds sends
    /// at most kind 3 and spawns nothing; after the launch the guardian
    /// always attempts kind 1 and enters its stop loop, which returns only
    /// from the settlement gate after the leader is reaped. A change that
    /// returns after the launch without a notice fails here.
    #[test]
    fn j2t_25_no_return_after_launch_without_a_notice() {
        // Comments removed, so prose neither satisfies nor breaks a check.
        let code: String = include_str!("guardian.rs")
            .lines()
            .map(|line| line.find("//").map_or(line, |at| &line[..at]))
            .collect::<Vec<_>>()
            .join("\n");
        let start = code.find("\nfn guardian(").expect("fn guardian");
        let body = &code[start..start + code[start..].find("\n}\n").expect("end")];
        let at = |needle: &str, from: usize| {
            from + body[from..]
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} after {from}"))
        };
        // `::exit(` and `::_exit(` name process exits, not `leader_exit(`.
        let exits = [
            "return",
            "?",
            "break",
            "panic!",
            ".unwrap()",
            ".expect(",
            "::exit(",
            "::_exit(",
        ];
        let assert_no_exit = |label: &str, text: &str| {
            for exit in exits {
                assert!(!text.contains(exit), "{label}: {exit} in {text}");
            }
        };
        assert_eq!(body.matches("spawn::launch(").count(), 1);
        assert_eq!(body.matches("kind: 1,").count(), 1);
        assert_eq!(body.matches("kind: 3,").count(), 1);
        let launch = at("spawn::launch(", 0);
        // The launch is the setup's last fallible step.
        let matched = at(
            "let (pid, mut table, mut env_marker) = match setup {",
            launch,
        );
        assert_no_exit("after the launch in setup", &body[launch..matched]);
        // The setup failure arm sends kind 3 only and spawns nothing.
        let ok_arm = at("Ok(spawned) => spawned,", matched);
        let err_arm = at("Err(error) => {", matched);
        assert!(ok_arm < err_arm);
        let arm_end = at("return Ok(());", err_arm);
        let arm = &body[err_arm..arm_end];
        assert!(arm.contains("kind: 3,"), "{arm}");
        assert_eq!(arm.matches("notice(").count(), 1, "{arm}");
        // From the match to the stop loop: kind 1 is attempted, no exit.
        let tail = arm_end + "return Ok(());".len();
        let loop_at = at("\n    loop {", tail);
        let kind_1 = at("kind: 1,", tail);
        assert!(kind_1 < loop_at);
        assert_no_exit("between the launch and the stop loop", &body[tail..loop_at]);
        // The stop loop returns once, from the settlement gate after the
        // leader is reaped; its fallible notices are kinds 6 and 2 there.
        let stop_loop = &body[loop_at..];
        let gate = stop_loop.find("Gate::Settled =>").expect("settlement gate");
        let reap = gate + stop_loop[gate..].find("waitpid(pid").expect("leader reap");
        assert_eq!(stop_loop.matches("return").count(), 1);
        assert!(stop_loop.find("return").unwrap() > reap);
        for (index, _) in stop_loop.match_indices('?') {
            assert!(index > reap, "fallible step before the gate: {index}");
        }
        for exit in ["panic!", ".unwrap()", ".expect(", "::exit(", "::_exit("] {
            assert!(!stop_loop.contains(exit), "{exit} in the stop loop");
        }
    }
}
