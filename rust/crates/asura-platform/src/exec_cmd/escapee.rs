//! EX5 escapee control for sandboxed jobs (escapee-control.md).
//!
//! The guardian attributes a job's descendants by the per-job sandbox marker:
//! a sandboxed process that may write the job's scratch (FD 9) and may not
//! write the Asura home `tmp` decoy (FD 10 + `/tmp`). Seatbelt membership is
//! inherited and permanent, so a `setsid` or double-fork escapee keeps it.
//! The sweep freezes the attributed set to a fixpoint, then kills it. Every
//! signal is sent only after the process's (pid, start time, pgid) tuple is
//! re-read and matches. No call here waits on a process or adds a time limit.
//!
//! A `!!` job (`person-outside-1`) has no sandbox. Its descendants are
//! attributed by the owned process group and by the job marker variable
//! `ASURA_JOB_<jobhex>=1` in their `KERN_PROCARGS2` environment. They are
//! counted after the shell exits and never signalled (EC-D1, PQ3).
use super::ExecCmdError;
use std::collections::HashSet;
use std::ffi::CString;

/// Most scans in one freeze phase (escapee-control.md, numeric limits).
pub(super) const FREEZE_ROUND_LIMIT: usize = 1024;
/// Accepted `kern.maxprocperuid` range for the PID buffer.
const MAX_UID_PROCESSES: usize = 1 << 20;
/// `PROC_UID_ONLY` from the installed `sys/proc_info.h`.
const PROC_UID_ONLY: u32 = 4;
/// `PROC_PIDFDPIPEINFO` from the installed `sys/proc_info.h`.
const PROC_PIDFDPIPEINFO: i32 = 6;
/// `SANDBOX_FILTER_PATH` from `libsystem_sandbox`.
const SANDBOX_FILTER_PATH: i32 = 1;
/// `struct pipe_fdinfo` is 184 bytes; `pipe_handle` is at byte 160 and
/// `pipe_peerhandle` at byte 168 (probe EC-V-GONE, installed SDK).
const PIPE_FDINFO_WORDS: usize = 23;
const PIPE_HANDLE_WORD: usize = 20;
const PIPE_PEER_WORD: usize = 21;

unsafe extern "C" {
    /// `int sandbox_check(pid_t, const char *operation, int type, ...)`,
    /// exported by `libsystem_sandbox` through libSystem.
    fn sandbox_check(pid: libc::pid_t, operation: *const libc::c_char, kind: i32, ...) -> i32;
    static SANDBOX_CHECK_NO_REPORT: i32;
}

/// A scan or signal result that cannot prove presence or absence (EC-D2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Unverifiable;

/// One attributed process, pinned by its start time and process group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Identity {
    pub pid: i32,
    pub start_sec: u64,
    pub start_usec: u64,
    pub pgid: u32,
    pub stopped: bool,
}
impl Identity {
    /// The same process: the tuple the guardian pinned still matches.
    fn same_process(&self, other: &Identity) -> bool {
        self.pid == other.pid
            && self.start_sec == other.start_sec
            && self.start_usec == other.start_usec
            && self.pgid == other.pgid
    }
}

/// The OS queries the sweep uses. `Libproc` is the real table; unit tests
/// supply a deterministic fake.
pub(super) trait Processes {
    /// Every PID of the caller's UID.
    fn list(&mut self) -> Result<Vec<i32>, Unverifiable>;
    /// The three-part marker test. A PID that no longer exists is not
    /// attributed (`sandbox_check` returns 1 for it, EC-V-GONE).
    fn attributed(&mut self, pid: i32) -> Result<bool, Unverifiable>;
    /// The pinned tuple; `None` when the process is gone.
    fn identity(&mut self, pid: i32) -> Result<Option<Identity>, Unverifiable>;
    /// Whether the process holds a capture pipe end. Unknown is `true`.
    fn holds_capture(&mut self, pid: i32) -> bool;
    /// Send `signal`; `Ok(false)` when the process is gone.
    fn signal(&mut self, pid: i32, signal: i32) -> Result<bool, Unverifiable>;
}

/// The attributed descendants found by one scan.
#[derive(Debug, Default)]
pub(super) struct Scan {
    pub members: Vec<Identity>,
    pub holders: usize,
}

/// One pass over the UID's PIDs, never including `exclude`.
pub(super) fn scan(table: &mut impl Processes, exclude: &[i32]) -> Result<Scan, Unverifiable> {
    let mut result = Scan::default();
    for pid in table.list()? {
        if pid <= 0 || exclude.contains(&pid) || !table.attributed(pid)? {
            continue;
        }
        if let Some(identity) = table.identity(pid)? {
            if table.holds_capture(pid) {
                result.holders += 1;
            }
            result.members.push(identity);
        }
    }
    Ok(result)
}

/// The escapees the guardian sent `SIGKILL` to, by PID and start time. At
/// most `cap` identities are kept; past that the count stays at `cap`.
#[derive(Debug)]
pub(super) struct Tally {
    killed: HashSet<(i32, u64, u64)>,
    cap: usize,
}
impl Tally {
    pub fn new(cap: usize) -> Self {
        Self {
            killed: HashSet::new(),
            cap,
        }
    }
    fn record(&mut self, identity: &Identity) {
        if self.killed.len() < self.cap {
            self.killed
                .insert((identity.pid, identity.start_sec, identity.start_usec));
        }
    }
    pub fn count(&self) -> u32 {
        u32::try_from(self.killed.len()).unwrap_or(u32::MAX)
    }
}

/// Re-read the tuple, then signal only the same process (PID-reuse check).
fn verified_signal(
    table: &mut impl Processes,
    pinned: &Identity,
    signal: i32,
) -> Result<bool, Unverifiable> {
    match table.identity(pinned.pid)? {
        Some(now) if now.same_process(pinned) => table.signal(pinned.pid, signal),
        // Gone, or a different process now holds the PID: never signal it.
        _ => Ok(false),
    }
}

/// The result of one sweep that could be verified.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Sweep {
    /// No attributed descendant and no capture-pipe holder remain.
    Clear,
    /// This many attributed descendants remain after the kill; sweep again.
    Remaining(usize),
}

/// Freeze-then-kill over the attributed set. `limit` is the freeze scan cap.
pub(super) fn sweep(
    table: &mut impl Processes,
    exclude: &[i32],
    tally: &mut Tally,
    limit: usize,
) -> Result<Sweep, Unverifiable> {
    let mut frozen = None;
    for _ in 0..limit {
        let found = scan(table, exclude)?;
        let mut running = found.members.iter().filter(|m| !m.stopped).peekable();
        if running.peek().is_none() {
            frozen = Some(found);
            break;
        }
        for member in running {
            verified_signal(table, member, libc::SIGSTOP)?;
        }
    }
    let capped = frozen.is_none();
    let set = match frozen {
        Some(found) => found,
        // Round cap: kill what is attributed now, then report unverifiable.
        None => scan(table, exclude)?,
    };
    if set.members.is_empty() && set.holders == 0 && !capped {
        return Ok(Sweep::Clear);
    }
    for member in &set.members {
        if verified_signal(table, member, libc::SIGKILL)? {
            tally.record(member);
        }
    }
    if capped {
        return Err(Unverifiable);
    }
    let after = scan(table, exclude)?;
    if after.members.is_empty() && after.holders == 0 {
        Ok(Sweep::Clear)
    } else {
        Ok(Sweep::Remaining(after.members.len()))
    }
}

/// The `!!` survivors found after the shell exited: live owned-group members
/// and env-marked processes, never signalled (EC-D1).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Survivors {
    pub count: u32,
    pub holders: usize,
}
impl Survivors {
    /// The kind 2 reason: the lease reason with no survivor; otherwise 14,
    /// or 15 when a survivor holds a capture pipe.
    pub fn settled_reason(&self, lease_reason: u8) -> u8 {
        if self.count == 0 {
            lease_reason
        } else if self.holders > 0 {
            super::protocol::REASON_ESCAPEES_HOLD_OUTPUT
        } else {
            super::protocol::REASON_ESCAPEES_LEFT_RUNNING
        }
    }
}
/// The `!!` attribution: the owned process group and the job marker
/// `ASURA_JOB_<jobhex>=1` in a process's `KERN_PROCARGS2` environment.
pub(super) struct EnvMarker {
    marker: Vec<u8>,
    /// A `kern.argmax` buffer, allocated before the command spawns.
    buffer: Vec<u8>,
    /// The owned process group: the leader's PID.
    group: i32,
}
impl EnvMarker {
    /// Setup before spawn; a failure refuses the job (`sandbox_unavailable`).
    pub fn new(marker: &str) -> Result<Self, ExecCmdError> {
        let argmax =
            usize::try_from(sysctl_int(c"kern.argmax")?).map_err(|_| ExecCmdError::Unavailable)?;
        if argmax == 0 || argmax > MAX_ARGUMENT_BYTES {
            return Err(ExecCmdError::Unavailable);
        }
        Ok(Self {
            marker: marker.as_bytes().to_vec(),
            buffer: vec![0; argmax],
            group: 0,
        })
    }
    /// Record the owned group once the leader exists.
    pub fn set_group(&mut self, leader: i32) {
        self.group = leader;
    }
    /// Whether `pid`'s `KERN_PROCARGS2` environment holds the marker. A
    /// process whose record cannot be read (gone, or a platform binary
    /// whose environment macOS 27 withholds from other processes) is not
    /// attributable, as for an env-scrubbed escapee (EXD-1).
    fn marked(&mut self, pid: i32) -> bool {
        let mut size = self.buffer.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        // SAFETY: the buffer holds `size` writable bytes; sysctl writes at
        // most that many and reports the count in `size`.
        let result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                self.buffer.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        result == 0
            && size <= self.buffer.len()
            && environment_holds(&self.buffer[..size], &self.marker)
    }
}
/// The process table seen through the `!!` attribution: listing, identity,
/// pipe holding and signals come from the real table; attribution is the
/// owned group or the environment marker, never the sandbox marker.
struct Outside<'a, P: Processes> {
    table: &'a mut P,
    env: &'a mut EnvMarker,
}
impl<P: Processes> Processes for Outside<'_, P> {
    fn list(&mut self) -> Result<Vec<i32>, Unverifiable> {
        self.table.list()
    }
    fn attributed(&mut self, pid: i32) -> Result<bool, Unverifiable> {
        let group = self.env.group;
        let in_group = self
            .table
            .identity(pid)?
            .is_some_and(|found| group > 0 && i64::from(found.pgid) == i64::from(group));
        Ok(in_group || self.env.marked(pid))
    }
    fn identity(&mut self, pid: i32) -> Result<Option<Identity>, Unverifiable> {
        self.table.identity(pid)
    }
    fn holds_capture(&mut self, pid: i32) -> bool {
        self.table.holds_capture(pid)
    }
    fn signal(&mut self, _pid: i32, _signal: i32) -> Result<bool, Unverifiable> {
        // EC-D1: a `!!` survivor is never signalled.
        Ok(false)
    }
}
/// One scan for `!!` survivors after the shell exited: live owned-group
/// members (PQ3) and env-marked processes, counted, never signalled.
/// Best effort (EXD-1): a scan that cannot be verified reports none, so
/// the job then settles on capture EOF or a cancel.
pub(super) fn survivors(
    table: &mut impl Processes,
    env: &mut EnvMarker,
    exclude: &[i32],
) -> Survivors {
    match scan(&mut Outside { table, env }, exclude) {
        Ok(found) => Survivors {
            count: u32::try_from(found.members.len()).unwrap_or(u32::MAX),
            holders: found.holders,
        },
        Err(Unverifiable) => Survivors::default(),
    }
}

/// The facts the settlement gate reads (escapee-control.md, settlement gate).
#[derive(Clone, Copy, Debug)]
pub(super) struct GateFacts {
    pub leader_exited: bool,
    pub group_empty: bool,
    /// `None` when the descendant scan was unverifiable.
    pub sweep: Option<Sweep>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Gate {
    /// Reap the leader and report `owned_group_settled`.
    Settled,
    /// Keep the leader fence and sweep again on the next pass.
    Wait,
    /// The scan cannot prove absence: report `cleanup_unconfirmed`.
    Unverifiable,
}
pub(super) fn gate(facts: GateFacts) -> Gate {
    if !facts.leader_exited || !facts.group_empty {
        return Gate::Wait;
    }
    match facts.sweep {
        Some(Sweep::Clear) => Gate::Settled,
        Some(Sweep::Remaining(_)) => Gate::Wait,
        None => Gate::Unverifiable,
    }
}

/// The marker (FD 9 scratch path) and decoy (FD 10 path + `/tmp`). Both come
/// from the guardian's pinned descriptors, never from command text; the
/// marker must lie directly inside the decoy directory.
pub(super) fn marker_and_decoy(
    scratch: &str,
    asura: &str,
) -> Result<(CString, CString), ExecCmdError> {
    let decoy = format!("{}/tmp", asura.trim_end_matches('/'));
    let name = scratch
        .strip_prefix(&decoy)
        .and_then(|rest| rest.strip_prefix('/'))
        .ok_or(ExecCmdError::Unavailable)?;
    if name.is_empty() || name.contains('/') || !asura.starts_with('/') {
        return Err(ExecCmdError::Unavailable);
    }
    Ok((
        CString::new(scratch).map_err(|_| ExecCmdError::Unavailable)?,
        CString::new(decoy).map_err(|_| ExecCmdError::Unavailable)?,
    ))
}

/// A `proc_listpids` byte count is complete only when it is non-negative,
/// whole PIDs, and smaller than the buffer (a full buffer may be cut).
pub(super) fn listed_count(bytes: i32, capacity_bytes: usize) -> Result<usize, Unverifiable> {
    let bytes = usize::try_from(bytes).map_err(|_| Unverifiable)?;
    if bytes >= capacity_bytes || !bytes.is_multiple_of(std::mem::size_of::<i32>()) {
        return Err(Unverifiable);
    }
    Ok(bytes / std::mem::size_of::<i32>())
}

/// The FD-list buffer for a reported size: `None` (treat as a holder) when the
/// size is odd or above the per-process limit.
pub(super) fn fd_entries(reported: i32, cap: usize) -> Option<usize> {
    let entry = std::mem::size_of::<libc::proc_fdinfo>();
    let bytes = usize::try_from(reported).ok()?;
    if !bytes.is_multiple_of(entry) {
        return None;
    }
    // Room for descriptors opened between the size and the list calls.
    let entries = bytes / entry + 16;
    (entries <= cap).then_some(entries)
}

fn errno() -> Option<i32> {
    std::io::Error::last_os_error().raw_os_error()
}
/// Clear errno so a following zero or negative result reads a fresh value.
fn clear_errno() {
    // SAFETY: __error returns this thread's errno location.
    unsafe {
        *libc::__error() = 0;
    }
}

/// Read a sysctl integer by name.
fn sysctl_int(name: &std::ffi::CStr) -> Result<i32, ExecCmdError> {
    let mut value: i32 = 0;
    let mut size = std::mem::size_of::<i32>();
    // SAFETY: fixed NUL-terminated name and a complete i32 output location.
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut value as *mut i32).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || size != std::mem::size_of::<i32>() {
        return Err(ExecCmdError::Unavailable);
    }
    Ok(value)
}

/// `PROC_PIDFDPIPEINFO` of one descriptor: (handle, peer handle).
fn pipe_handles(pid: i32, fd: i32) -> Result<Option<(u64, u64)>, ()> {
    let mut info = [0u64; PIPE_FDINFO_WORDS];
    let size = std::mem::size_of_val(&info) as i32;
    clear_errno();
    // SAFETY: complete, aligned output buffer of the stated size.
    let got = unsafe {
        libc::proc_pidfdinfo(pid, fd, PROC_PIDFDPIPEINFO, info.as_mut_ptr().cast(), size)
    };
    if got == size {
        Ok(Some((info[PIPE_HANDLE_WORD], info[PIPE_PEER_WORD])))
    } else if got <= 0 && matches!(errno(), Some(libc::ESRCH) | Some(libc::EBADF)) {
        Ok(None)
    } else {
        Err(())
    }
}

/// The capture write ends' pipe handles, recorded before the command spawns.
pub(super) fn capture_handles(fds: [i32; 2]) -> Result<Vec<u64>, ExecCmdError> {
    // SAFETY: getpid has no preconditions.
    let own = unsafe { libc::getpid() };
    let mut handles = Vec::with_capacity(4);
    for fd in fds {
        match pipe_handles(own, fd) {
            Ok(Some((handle, peer))) if handle != 0 => handles.extend([handle, peer]),
            _ => return Err(ExecCmdError::Unavailable),
        }
    }
    Ok(handles)
}

/// The real process table: libproc and `sandbox_check` for one job.
pub(super) struct Libproc {
    uid: u32,
    pids: Vec<i32>,
    marker: CString,
    decoy: CString,
    capture: Vec<u64>,
    fd_cap: usize,
    #[cfg(feature = "test-support")]
    pub(super) fault: Fault,
}
impl Libproc {
    /// Setup before spawn; any failure refuses the job (`sandbox_unavailable`).
    pub fn new(scratch: &str, asura: &str, capture: Vec<u64>) -> Result<Self, ExecCmdError> {
        let (marker, decoy) = marker_and_decoy(scratch, asura)?;
        let limit = usize::try_from(sysctl_int(c"kern.maxprocperuid")?)
            .map_err(|_| ExecCmdError::Unavailable)?;
        let fd_cap = usize::try_from(sysctl_int(c"kern.maxfilesperproc")?)
            .map_err(|_| ExecCmdError::Unavailable)?;
        if limit == 0 || limit > MAX_UID_PROCESSES || fd_cap == 0 {
            return Err(ExecCmdError::Unavailable);
        }
        Ok(Self {
            // SAFETY: geteuid has no preconditions.
            uid: unsafe { libc::geteuid() },
            // One spare entry: a full buffer is never read as complete.
            pids: vec![0; limit + 1],
            marker,
            decoy,
            capture,
            fd_cap,
            #[cfg(feature = "test-support")]
            fault: Fault::None,
        })
    }
    /// The scan limit: the most PIDs one scan can return.
    pub fn limit(&self) -> usize {
        self.pids.len() - 1
    }
    fn check(&self, pid: i32, path: &CString) -> i32 {
        // SAFETY: fixed operation string and a live NUL-terminated path; the
        // variadic argument is the path pointer SANDBOX_FILTER_PATH expects.
        unsafe {
            sandbox_check(
                pid,
                c"file-write-data".as_ptr(),
                SANDBOX_FILTER_PATH | SANDBOX_CHECK_NO_REPORT,
                path.as_ptr(),
            )
        }
    }
}
impl Processes for Libproc {
    fn list(&mut self) -> Result<Vec<i32>, Unverifiable> {
        #[cfg(feature = "test-support")]
        if let Fault::Unverifiable(remaining) = &mut self.fault
            && *remaining > 0
        {
            *remaining -= 1;
            return Err(Unverifiable);
        }
        let capacity = std::mem::size_of_val(self.pids.as_slice());
        // SAFETY: the buffer is complete and its byte size is stated exactly.
        let bytes = unsafe {
            libc::proc_listpids(
                PROC_UID_ONLY,
                self.uid,
                self.pids.as_mut_ptr().cast(),
                i32::try_from(capacity).map_err(|_| Unverifiable)?,
            )
        };
        let count = listed_count(bytes, capacity)?;
        Ok(self.pids[..count].to_vec())
    }
    fn attributed(&mut self, pid: i32) -> Result<bool, Unverifiable> {
        // SAFETY: a NULL operation with no filter asks only whether the
        // process is sandboxed; no variadic argument is read.
        match unsafe { sandbox_check(pid, std::ptr::null(), 0) } {
            0 => return Ok(false),
            1 => {}
            _ => return Err(Unverifiable),
        }
        match self.check(pid, &self.marker) {
            0 => {}
            1 => return Ok(false),
            _ => return Err(Unverifiable),
        }
        match self.check(pid, &self.decoy) {
            1 => Ok(true),
            0 => Ok(false),
            _ => Err(Unverifiable),
        }
    }
    fn identity(&mut self, pid: i32) -> Result<Option<Identity>, Unverifiable> {
        // SAFETY: plain-data output; zero is a valid bit pattern.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        clear_errno();
        // SAFETY: complete output buffer of the stated size.
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if got == size {
            if info.pbi_status == libc::SZOMB {
                return Ok(None);
            }
            return Ok(Some(Identity {
                pid,
                start_sec: info.pbi_start_tvsec,
                start_usec: info.pbi_start_tvusec,
                pgid: info.pbi_pgid,
                stopped: info.pbi_status == libc::SSTOP,
            }));
        }
        if got <= 0 && errno() == Some(libc::ESRCH) {
            Ok(None)
        } else {
            Err(Unverifiable)
        }
    }
    fn holds_capture(&mut self, pid: i32) -> bool {
        clear_errno();
        // SAFETY: a NULL buffer with size 0 asks only for the size.
        let reported =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if reported <= 0 {
            // No descriptors, or gone; any other failure counts as a holder.
            return !matches!(errno(), Some(0) | Some(libc::ESRCH));
        }
        let Some(entries) = fd_entries(reported, self.fd_cap) else {
            return true;
        };
        let mut fds = vec![
            libc::proc_fdinfo {
                proc_fd: 0,
                proc_fdtype: 0,
            };
            entries
        ];
        let capacity = std::mem::size_of_val(fds.as_slice());
        let Ok(capacity_i32) = i32::try_from(capacity) else {
            return true;
        };
        clear_errno();
        // SAFETY: complete buffer of `entries` records, byte size stated.
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                capacity_i32,
            )
        };
        if got <= 0 {
            return !matches!(errno(), Some(0) | Some(libc::ESRCH));
        }
        let entry = std::mem::size_of::<libc::proc_fdinfo>();
        let got = got as usize;
        if got >= capacity || !got.is_multiple_of(entry) {
            return true;
        }
        for fd in &fds[..got / entry] {
            if fd.proc_fdtype != libc::PROX_FDTYPE_PIPE as u32 {
                continue;
            }
            match pipe_handles(pid, fd.proc_fd) {
                Ok(Some((handle, peer))) => {
                    if self.capture.contains(&handle) || self.capture.contains(&peer) {
                        return true;
                    }
                }
                Ok(None) => {}
                Err(()) => return true,
            }
        }
        false
    }
    fn signal(&mut self, pid: i32, signal: i32) -> Result<bool, Unverifiable> {
        #[cfg(feature = "test-support")]
        if self.fault == Fault::Undyable && signal == libc::SIGKILL {
            // As for a process that cannot be killed: reported as sent.
            return Ok(true);
        }
        clear_errno();
        // SAFETY: the caller re-verified this PID's pinned tuple just now.
        if unsafe { libc::kill(pid, signal) } == 0 {
            Ok(true)
        } else if errno() == Some(libc::ESRCH) {
            Ok(false)
        } else {
            Err(Unverifiable)
        }
    }
}

/// Largest accepted `kern.argmax`.
const MAX_ARGUMENT_BYTES: usize = 16 << 20;
/// Whether a `KERN_PROCARGS2` record's environment holds `entry` exactly:
/// argc, the executable path and its NUL padding, argc strings, then the
/// environment strings up to the first empty one.
pub(super) fn environment_holds(record: &[u8], entry: &[u8]) -> bool {
    let Some(argc) = record.get(..4) else {
        return false;
    };
    let argc = i32::from_ne_bytes([argc[0], argc[1], argc[2], argc[3]]);
    let Ok(argc) = usize::try_from(argc) else {
        return false;
    };
    let mut rest = &record[4..];
    let path = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
    rest = &rest[path..];
    let padding = rest.iter().position(|b| *b != 0).unwrap_or(rest.len());
    rest = &rest[padding..];
    let mut strings = rest.split(|b| *b == 0);
    for _ in 0..argc {
        if strings.next().is_none() {
            return false;
        }
    }
    strings.take_while(|s| !s.is_empty()).any(|s| s == entry)
}

/// Test-support guardian faults (escapee-control.md, test-support faults).
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Fault {
    None,
    /// Skip the sweep: the old group-only check (EC-V-OLD).
    GroupOnly,
    /// `SIGKILL` to escapees is suppressed (EC-I8).
    Undyable,
    /// The first n scans are unverifiable (EC-D2).
    Unverifiable(u32),
    /// `/usr/bin/sandbox-exec` is unavailable (PC-I29).
    NoSandboxExec,
}
#[cfg(feature = "test-support")]
impl Fault {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "group-only" => Some(Self::GroupOnly),
            "undyable" => Some(Self::Undyable),
            "no-sandbox-exec" => Some(Self::NoSandboxExec),
            _ => text
                .strip_prefix("unverifiable:")
                .and_then(|n| n.parse().ok())
                .map(Self::Unverifiable),
        }
    }
}

/// EC-I11: one full scan with the real table for a live job's marker and
/// decoy, from a test process. Returns the attributed PIDs.
#[cfg(feature = "test-support")]
pub fn scan_for_test(scratch: &str, asura: &str) -> Result<Vec<i32>, ExecCmdError> {
    let mut table = Libproc::new(scratch, asura, Vec::new())?;
    // SAFETY: getpid has no preconditions.
    let own = unsafe { libc::getpid() };
    scan(&mut table, &[own])
        .map(|found| found.members.iter().map(|m| m.pid).collect())
        .map_err(|_| ExecCmdError::CleanupUnconfirmed)
}

#[cfg(test)]
mod tests;
