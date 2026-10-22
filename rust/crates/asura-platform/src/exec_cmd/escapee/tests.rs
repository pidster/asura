//! EC-U1, EC-U3–EC-U6 with a deterministic process table: no real process
//! is listed, queried or signalled.
use super::*;

#[derive(Clone, Debug)]
struct Proc {
    identity: Identity,
    attributed: bool,
    holds: bool,
    alive: bool,
    ignores_stop: bool,
    ignores_kill: bool,
    /// While running, each scan of this process starts one new attributed
    /// child, up to this many (a forking escapee).
    forks: usize,
}
fn proc(pid: i32) -> Proc {
    Proc {
        identity: Identity {
            pid,
            start_sec: 1_000 + pid as u64,
            start_usec: 7,
            pgid: pid as u32,
            stopped: false,
        },
        attributed: true,
        holds: false,
        alive: true,
        ignores_stop: false,
        ignores_kill: false,
        forks: 0,
    }
}
#[derive(Default)]
struct Fake {
    procs: Vec<Proc>,
    signals: Vec<(i32, i32)>,
    scans: usize,
    next_pid: i32,
    list_fault: bool,
    attribution_fault: Option<i32>,
    identity_fault: Option<i32>,
    signal_fault: bool,
    /// After the first identity read of this PID, a different process
    /// (new start time and pgid, not attributed) holds it.
    reuse: Option<i32>,
    reads: std::collections::HashMap<i32, usize>,
}
impl Fake {
    fn with(procs: Vec<Proc>) -> Self {
        Self {
            procs,
            next_pid: 900,
            ..Self::default()
        }
    }
    fn find(&mut self, pid: i32) -> Option<&mut Proc> {
        self.procs.iter_mut().find(|p| p.identity.pid == pid)
    }
    fn signalled(&self, pid: i32) -> Vec<i32> {
        self.signals
            .iter()
            .filter(|(p, _)| *p == pid)
            .map(|(_, s)| *s)
            .collect()
    }
}
impl Processes for Fake {
    fn list(&mut self) -> Result<Vec<i32>, Unverifiable> {
        if self.list_fault {
            return Err(Unverifiable);
        }
        self.scans += 1;
        let mut born = Vec::new();
        for p in self.procs.iter_mut().filter(|p| p.alive) {
            if !p.identity.stopped && p.forks > 0 {
                p.forks -= 1;
                born.push(p.identity.pid);
            }
        }
        for _ in born {
            self.next_pid += 1;
            self.procs.push(proc(self.next_pid));
        }
        Ok(self
            .procs
            .iter()
            .filter(|p| p.alive)
            .map(|p| p.identity.pid)
            .chain([1, 0, -1])
            .collect())
    }
    fn attributed(&mut self, pid: i32) -> Result<bool, Unverifiable> {
        if self.attribution_fault == Some(pid) {
            return Err(Unverifiable);
        }
        Ok(self.find(pid).is_some_and(|p| p.alive && p.attributed))
    }
    fn identity(&mut self, pid: i32) -> Result<Option<Identity>, Unverifiable> {
        if self.identity_fault == Some(pid) {
            return Err(Unverifiable);
        }
        let reads = {
            let n = self.reads.entry(pid).or_default();
            *n += 1;
            *n
        };
        if self.reuse == Some(pid) && reads > 1 {
            let p = self.find(pid).expect("reused pid");
            p.identity.start_sec += 50;
            p.identity.pgid += 1;
            p.attributed = false;
            p.forks = 0;
            self.reuse = None;
        }
        Ok(self.find(pid).filter(|p| p.alive).map(|p| p.identity))
    }
    fn holds_capture(&mut self, pid: i32) -> bool {
        self.find(pid).is_some_and(|p| p.holds)
    }
    fn signal(&mut self, pid: i32, signal: i32) -> Result<bool, Unverifiable> {
        if self.signal_fault {
            return Err(Unverifiable);
        }
        self.signals.push((pid, signal));
        let Some(p) = self.find(pid).filter(|p| p.alive) else {
            return Ok(false);
        };
        match signal {
            libc::SIGSTOP if !p.ignores_stop => p.identity.stopped = true,
            libc::SIGKILL if !p.ignores_kill => p.alive = false,
            _ => {}
        }
        Ok(true)
    }
}

/// EC-U1: the gate settles only when the leader exited, the group is empty
/// and the sweep is clear; any other input waits or is unverifiable.
#[test]
fn ec_u1_gate_settles_only_when_every_input_holds() {
    let facts = |leader_exited, group_empty, sweep| GateFacts {
        leader_exited,
        group_empty,
        sweep,
    };
    assert_eq!(gate(facts(true, true, Some(Sweep::Clear))), Gate::Settled);
    for (label, input, expected) in [
        (
            "leader live",
            facts(false, true, Some(Sweep::Clear)),
            Gate::Wait,
        ),
        (
            "group left",
            facts(true, false, Some(Sweep::Clear)),
            Gate::Wait,
        ),
        (
            "escapee left",
            facts(true, true, Some(Sweep::Remaining(1))),
            Gate::Wait,
        ),
        ("unverifiable", facts(true, true, None), Gate::Unverifiable),
        ("nothing", facts(false, false, None), Gate::Wait),
    ] {
        assert_eq!(gate(input), expected, "{label}");
    }
}

/// Freeze-then-kill: every attributed process is stopped before any kill,
/// the pipe holder is in the set, and the sweep ends clear with a count.
#[test]
fn sweep_freezes_all_then_kills_and_counts() {
    let mut holder = proc(11);
    holder.holds = true;
    let mut other = proc(12);
    other.attributed = false;
    let mut table = Fake::with(vec![proc(10), holder, other, proc(13)]);
    let mut tally = Tally::new(100);
    let result = sweep(&mut table, &[13], &mut tally, FREEZE_ROUND_LIMIT);
    assert_eq!(result, Ok(Sweep::Clear));
    assert_eq!(table.signalled(10), [libc::SIGSTOP, libc::SIGKILL]);
    assert_eq!(table.signalled(11), [libc::SIGSTOP, libc::SIGKILL]);
    assert!(table.signalled(12).is_empty(), "unattributed");
    assert!(table.signalled(13).is_empty(), "excluded leader");
    let first_kill = table
        .signals
        .iter()
        .position(|(_, s)| *s == libc::SIGKILL)
        .unwrap();
    assert!(
        table.signals[..first_kill]
            .iter()
            .all(|(_, s)| *s == libc::SIGSTOP)
    );
    assert_eq!(tally.count(), 2);
    // A second sweep finds nothing and adds nothing.
    assert_eq!(
        sweep(&mut table, &[13], &mut tally, FREEZE_ROUND_LIMIT),
        Ok(Sweep::Clear)
    );
    assert_eq!(tally.count(), 2);
}

/// EC-I5 unit: a forking escapee cannot outrun the fixpoint; each child it
/// starts before it is stopped is found and frozen in a later round.
#[test]
fn sweep_reaches_a_fixpoint_over_a_forking_escapee() {
    let mut forker = proc(20);
    forker.forks = 3;
    let mut table = Fake::with(vec![forker]);
    let mut tally = Tally::new(100);
    assert_eq!(
        sweep(&mut table, &[], &mut tally, FREEZE_ROUND_LIMIT),
        Ok(Sweep::Clear)
    );
    assert_eq!(table.procs.len(), 2, "one child born before the stop");
    assert!(table.procs.iter().all(|p| !p.alive));
    assert_eq!(tally.count(), 2);
}

/// EC-U3: a PID reused between the scan and the signal is never signalled;
/// the next scan decides again.
#[test]
fn ec_u3_reused_pid_is_not_signalled() {
    let mut table = Fake::with(vec![proc(30)]);
    table.reuse = Some(30);
    let mut tally = Tally::new(100);
    assert_eq!(
        sweep(&mut table, &[], &mut tally, FREEZE_ROUND_LIMIT),
        Ok(Sweep::Clear)
    );
    assert!(table.signals.is_empty(), "{:?}", table.signals);
    assert_eq!(tally.count(), 0);
}

/// EC-U3: a process that exits between the scan and its signal is not
/// signalled and not counted.
#[test]
fn ec_u3_gone_before_signal_is_not_signalled() {
    let mut table = Fake::with(vec![proc(31)]);
    let pinned = table.procs[0].identity;
    table.procs[0].alive = false;
    assert_eq!(
        verified_signal(&mut table, &pinned, libc::SIGKILL),
        Ok(false)
    );
    assert!(table.signals.is_empty());
}

/// EC-U4: a process that never stops reaches the freeze cap; the sweep
/// still kills it and reports unverifiable, never loops without end.
#[test]
fn ec_u4_freeze_round_cap_kills_then_reports_unverifiable() {
    let mut stubborn = proc(40);
    stubborn.ignores_stop = true;
    let mut table = Fake::with(vec![stubborn]);
    let mut tally = Tally::new(100);
    assert_eq!(sweep(&mut table, &[], &mut tally, 8), Err(Unverifiable));
    assert_eq!(table.scans, 9, "8 freeze scans and one kill scan");
    assert_eq!(table.signalled(40).last(), Some(&libc::SIGKILL));
    assert_eq!(tally.count(), 1);
    assert!(!table.procs[0].alive);
    // A pathological forker that keeps adding children also stops at the cap.
    let mut forker = proc(41);
    forker.ignores_stop = true;
    forker.forks = usize::MAX;
    let mut table = Fake::with(vec![forker]);
    assert_eq!(
        sweep(&mut table, &[], &mut Tally::new(100), 16),
        Err(Unverifiable)
    );
    assert_eq!(table.scans, 17);
}

/// EC-I8 unit: a process that survives SIGKILL leaves the set non-empty;
/// the sweep never reports clear while it lives.
#[test]
fn undying_escapee_keeps_the_gate_closed() {
    let mut undying = proc(50);
    undying.ignores_kill = true;
    let mut table = Fake::with(vec![undying]);
    let mut tally = Tally::new(100);
    for _ in 0..3 {
        assert_eq!(
            sweep(&mut table, &[], &mut tally, FREEZE_ROUND_LIMIT),
            Ok(Sweep::Remaining(1))
        );
    }
    assert_eq!(tally.count(), 1, "distinct identities");
    table.procs[0].alive = false;
    assert_eq!(
        sweep(&mut table, &[], &mut tally, FREEZE_ROUND_LIMIT),
        Ok(Sweep::Clear)
    );
}

/// EC-U5: every unverifiable query makes the sweep unverifiable, never
/// clear; buffer sizes that are over-size or not whole entries are refused.
#[test]
fn ec_u5_faults_and_odd_buffers_are_unverifiable() {
    type Inject = fn(&mut Fake);
    let faults: [(&str, Inject); 4] = [
        ("list", |t| t.list_fault = true),
        ("attribution", |t| t.attribution_fault = Some(60)),
        ("identity", |t| t.identity_fault = Some(60)),
        ("signal", |t| t.signal_fault = true),
    ];
    for (label, fault) in faults {
        let mut table = Fake::with(vec![proc(60)]);
        fault(&mut table);
        assert_eq!(
            sweep(&mut table, &[], &mut Tally::new(10), FREEZE_ROUND_LIMIT),
            Err(Unverifiable),
            "{label}"
        );
    }
    assert_eq!(listed_count(16, 64), Ok(4));
    assert_eq!(listed_count(0, 64), Ok(0));
    for (bytes, capacity) in [(-1, 64), (6, 64), (64, 64), (68, 64)] {
        assert_eq!(listed_count(bytes, capacity), Err(Unverifiable), "{bytes}");
    }
    assert_eq!(fd_entries(80, 100), Some(26));
    assert_eq!(fd_entries(81, 100), None, "odd size");
    assert_eq!(fd_entries(8 * 100, 100), None, "over the per-process cap");
    assert_eq!(fd_entries(-8, 100), None);
}

/// The count keeps distinct identities and stops at the scan limit.
#[test]
fn tally_counts_distinct_identities_up_to_the_scan_limit() {
    let mut tally = Tally::new(2);
    let a = proc(70).identity;
    tally.record(&a);
    tally.record(&a);
    assert_eq!(tally.count(), 1);
    let mut reused = a;
    reused.start_sec += 1;
    tally.record(&reused);
    tally.record(&proc(71).identity);
    assert_eq!(tally.count(), 2);
}

/// EC-U6: the marker is the pinned scratch path and the decoy is the pinned
/// Asura home `tmp`; any other shape refuses setup.
#[test]
fn ec_u6_marker_and_decoy_come_from_pinned_paths() {
    let (marker, decoy) = marker_and_decoy("/h/.asura/tmp/exec-cmd-ab", "/h/.asura").unwrap();
    assert_eq!(marker.to_str().unwrap(), "/h/.asura/tmp/exec-cmd-ab");
    assert_eq!(decoy.to_str().unwrap(), "/h/.asura/tmp");
    for (scratch, asura) in [
        ("/h/.asura/tmp", "/h/.asura"),
        ("/h/.asura/tmp/", "/h/.asura"),
        ("/h/.asura/tmpx/a", "/h/.asura"),
        ("/h/.asura/tmp/a/b", "/h/.asura"),
        ("/h/project/a", "/h/.asura"),
        ("h/.asura/tmp/a", "h/.asura"),
        ("/h/.asura/tmp/a\0b", "/h/.asura"),
    ] {
        assert_eq!(
            marker_and_decoy(scratch, asura).err(),
            Some(ExecCmdError::Unavailable),
            "{scratch}"
        );
    }
}

/// The installed SDK layout the FFI relies on (probe EC-V-GONE).
#[test]
fn sdk_layout_matches_the_recorded_probe() {
    assert_eq!(std::mem::size_of::<libc::proc_bsdinfo>(), 136);
    assert_eq!(std::mem::size_of::<libc::proc_fdinfo>(), 8);
    assert_eq!(PIPE_FDINFO_WORDS * 8, 184);
    assert_eq!((PIPE_HANDLE_WORD * 8, PIPE_PEER_WORD * 8), (160, 168));
    // SAFETY: reading an exported constant.
    assert_eq!(unsafe { SANDBOX_CHECK_NO_REPORT }, 0x4000_0000);
}

/// The real table on this process: unsandboxed, so never attributed; a
/// PID that does not exist is not attributed either (EC-V-GONE).
#[test]
fn real_table_never_attributes_unsandboxed_or_absent_processes() {
    let mut table = Libproc {
        uid: unsafe { libc::geteuid() },
        pids: vec![0; 16],
        marker: CString::new("/").unwrap(),
        decoy: CString::new("/private/var/empty").unwrap(),
        capture: Vec::new(),
        fd_cap: 1024,
        #[cfg(feature = "test-support")]
        fault: Fault::None,
    };
    let own = unsafe { libc::getpid() };
    assert_eq!(table.attributed(own), Ok(false));
    assert_eq!(table.identity(own).unwrap().map(|i| i.pid), Some(own));
    assert_eq!(table.attributed(i32::MAX - 1), Ok(false));
    assert_eq!(table.identity(i32::MAX - 1), Ok(None));
    assert!(!table.holds_capture(i32::MAX - 1));
    assert_eq!(table.signal(i32::MAX - 1, 0), Ok(false));
    // A 16-entry buffer is too small for this UID's processes: a full
    // buffer is unverifiable, never a complete list.
    assert_eq!(table.list(), Err(Unverifiable));
}
