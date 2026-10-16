//! J1 worker-layer qualification: real processes, the real guardian and
//! sandbox, real capture workers and the real payload store in private
//! scratch homes. Each case ends with the raw-byte sweep.
use asura_platform::RuntimeDirectory;
use asura_platform::secrets::{SecretVariable, rewrite_shell_call};
use asura_service::test_support::{Job, Outcome, run_jobs};
use asura_storage::authority::conversation::{CaptureReason, CaptureState, Id, LossReason};
use asura_storage::payload::{
    MIB, Origin, PagePool, PageRequest, PageState, PayloadStore, read_page,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Home {
    path: PathBuf,
    runtime: RuntimeDirectory,
    store: Arc<PayloadStore>,
    installation: Id,
}
impl Home {
    fn new(label: &str) -> Result<Self> {
        Self::at(&PathBuf::from("/private/tmp"), label)
    }
    fn at(parent: &Path, label: &str) -> Result<Self> {
        let name: String = asura_platform::random_id()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let path = parent.join(format!("as-cw-{label}-{}", &name[..12]));
        fs::create_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        let path = fs::canonicalize(path)?;
        fs::create_dir(path.join("project"))?;
        let runtime = RuntimeDirectory::scratch(&path, true)?;
        let store = PayloadStore::open(&runtime).map_err(|e| format!("store: {e:?}"))?;
        store
            .set_quota(asura_storage::payload::DEFAULT_QUOTA)
            .map_err(|e| format!("quota: {e:?}"))?;
        Ok(Self {
            path,
            runtime,
            store: Arc::new(store),
            installation: asura_platform::random_id(),
        })
    }
    fn project(&self) -> String {
        self.path.join("project").to_string_lossy().into_owned()
    }
    fn run(&self, jobs: Vec<Job>, probe: bool, cancel: Option<Duration>) -> Result<Vec<Outcome>> {
        Ok(run_jobs(
            &self.runtime,
            &self.project(),
            &self.store,
            self.installation,
            jobs,
            probe,
            cancel,
            Duration::from_secs(120),
        )?)
    }
    fn one(&self, command: &str) -> Result<Outcome> {
        let (command, secrets) = rewritten(command);
        Ok(self
            .run(vec![Job { command, secrets }], false, None)?
            .remove(0))
    }
    fn object(&self, id: Id) -> Result<Vec<u8>> {
        Ok(fs::read(
            self.path.join(".asura/payloads/objects").join(hex(id)),
        )?)
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
fn hex(id: Id) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}
fn rewritten(command: &str) -> (String, Vec<SecretVariable>) {
    let rewrite = rewrite_shell_call(command, ".").unwrap_or_else(|r| panic!("{r}"));
    (rewrite.command, rewrite.variables)
}
/// Fixture secrets assembled at run time, so no token is committed.
fn token(seed: &str) -> String {
    ["ghp_", &seed.repeat(12)].concat()
}
fn holds(bytes: &[u8], needle: &str) -> bool {
    bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
}
/// The raw-byte sweep: no file under the home holds a fixture secret.
fn sweep(root: &Path, secrets: &[&str]) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            sweep(&path, secrets)?;
        } else if meta.is_file() {
            let bytes = fs::read(&path)?;
            for secret in secrets {
                if holds(&bytes, secret) {
                    return Err(format!("raw secret bytes in {}", path.display()).into());
                }
            }
        }
    }
    Ok(())
}
fn check_digests(home: &Home, outcome: &Outcome) -> Result<()> {
    for (i, stream) in [&outcome.settled.stdout, &outcome.settled.stderr]
        .into_iter()
        .enumerate()
    {
        let bytes = home.object(outcome.payloads[i])?;
        assert_eq!(bytes.len() as u64, stream.stored_bytes);
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        assert_eq!(Some(digest), stream.digest, "digest of stored bytes");
    }
    Ok(())
}
fn wait_installed(home: &Home, outcome: &Outcome) -> Result<()> {
    let end = Instant::now() + Duration::from_secs(10);
    while !home
        .path
        .join(".asura/payloads/objects")
        .join(hex(outcome.payloads[1]))
        .exists()
    {
        if Instant::now() > end {
            return Err("install".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// J1T-19 (RG-01): interleaved streams far larger than the old rings are
/// stored whole; stored bytes equal the redacted source; digests match.
fn full_capture() -> Result<()> {
    let home = Home::new("full")?;
    let outcome = home.one(
        "i=0; while [ $i -lt 3000 ]; do printf 'out-%06d-abcdefghijklmnopqrstuvwxyz\\n' $i; \
         printf 'err-%06d\\n' $i >&2; i=$((i+1)); done",
    )?;
    assert_eq!(
        outcome.settled.state,
        CaptureState::Complete,
        "{:?}",
        outcome.settled
    );
    assert_eq!(outcome.result.status, 1);
    let mut expected_out = String::new();
    let mut expected_err = String::new();
    for i in 0..3000 {
        expected_out.push_str(&format!("out-{i:06}-abcdefghijklmnopqrstuvwxyz\n"));
        expected_err.push_str(&format!("err-{i:06}\n"));
    }
    wait_installed(&home, &outcome)?;
    assert_eq!(home.object(outcome.payloads[0])?, expected_out.as_bytes());
    assert_eq!(home.object(outcome.payloads[1])?, expected_err.as_bytes());
    assert_eq!(
        outcome.settled.stdout.observed_bytes,
        expected_out.len() as u64
    );
    assert!(outcome.settled.stdout.eof && outcome.settled.stderr.eof);
    check_digests(&home, &outcome)?;
    // The model tail is the last 6,144 bytes of the redacted stream.
    assert!(outcome.result.text.contains("capture=complete"));
    assert!(outcome.result.text.contains("out-002999-"));
    assert!(outcome.result.truncated);
    println!(
        "PASS J1T-19 full capture: {} + {} bytes stored whole",
        expected_out.len(),
        expected_err.len()
    );
    Ok(())
}

/// J1T-12 (SDI-01): a secret printed in pieces, in separate pipe writes,
/// never appears in a `live` page; the stored stream holds the placeholder.
fn split_secret() -> Result<()> {
    let home = Home::new("split")?;
    let secret = token("pQ7");
    let (a, b, c) = (&secret[..6], &secret[6..20], &secret[20..]);
    let command = format!(
        "printf 'before\\n'; printf 'token: {a}'; sleep 0.4; printf '{b}'; sleep 0.4; printf '{c}\\n'; sleep 0.3; printf 'after\\n'"
    );
    let outcomes = home.run(
        vec![Job {
            command,
            secrets: Vec::new(),
        }],
        true,
        None,
    )?;
    let outcome = &outcomes[0];
    assert_eq!(outcome.settled.state, CaptureState::Complete);
    let mut live = 0;
    for page in &outcome.live_pages {
        for piece in [a, b, c] {
            assert!(!page.text.contains(piece), "live page showed a piece");
        }
        if page.state == PageState::Live && !page.text.is_empty() {
            live += 1;
        }
    }
    assert!(live > 0, "pages were read while live");
    wait_installed(&home, outcome)?;
    let stored = home.object(outcome.payloads[0])?;
    let text = String::from_utf8_lossy(&stored);
    assert!(text.contains("before\n") && text.contains("after\n"));
    assert!(text.contains("$ASURA_SECRET_1"), "{text}");
    assert!(!outcome.result.text.contains(&secret));
    sweep(&home.path, &[&secret])?;
    println!("PASS J1T-12 split secret: {live} live pages, no piece shown");
    Ok(())
}

/// J1T-13 (SDI-02): a private key block, a line over 64 KiB and binary data
/// are stored as SDU-06, SDU-07 and SDU-09 specify; the digest matches.
fn key_long_binary() -> Result<()> {
    let home = Home::new("klb")?;
    let project = home.path.join("project");
    let begin = ["-----BEGIN ", "RSA ", "PRIVATE", " KEY-----"].concat();
    let end = ["-----END ", "RSA ", "PRIVATE", " KEY-----"].concat();
    let body = ["MIIE", "owIBAAKCAQEA", &"Zx9Qw".repeat(10)].concat();
    fs::write(
        project.join("key.pem"),
        format!("{begin}\n{body}\n{body}\n{end}\n"),
    )?;
    let mut long = vec![b'L'; 70 * 1024];
    long.push(b'\n');
    fs::write(project.join("long.txt"), &long)?;
    let binary: Vec<u8> = (0..=255u8).collect();
    fs::write(project.join("bin.dat"), &binary)?;
    let outcome = home.one("cat key.pem; cat long.txt; cat bin.dat")?;
    assert_eq!(outcome.settled.state, CaptureState::Complete);
    wait_installed(&home, &outcome)?;
    let stored = home.object(outcome.payloads[0])?;
    assert!(!holds(&stored, &body) && !holds(&stored, &begin));
    assert!(holds(&stored, "$ASURA_SECRET_"), "key placeholder");
    let marker = format!(
        "[ASURA_WITHHELD {} bytes: line_over_limit]",
        70 * 1024 - 64 * 1024 + 64 * 1024
    );
    let spans = &outcome.settled.stdout.spans;
    assert_eq!(spans.len(), 1, "{spans:?}");
    assert_eq!(spans[0].reason, LossReason::LineOverLimit);
    assert!(holds(&stored, "[ASURA_WITHHELD "), "loss marker");
    let _ = marker;
    assert!(stored.ends_with(&binary), "binary bytes kept unchanged");
    check_digests(&home, &outcome)?;
    println!(
        "PASS J1T-13 key, long line and binary: withheld {} bytes, digest matches",
        outcome.settled.stdout.withheld_bytes
    );
    Ok(())
}

/// J1T-16 (SDI-07 re-run), J1T-17 (SDI-08), J1T-14 at the worker layer:
/// `set` and echoes on both streams store the variable's placeholder.
fn command_secrets() -> Result<()> {
    let home = Home::new("cmd")?;
    let secret = token("Kd4");
    let outcome = home.one(&format!(
        "printf '%s' \"{secret}\" > out.txt; printf '%s\\n' \"{secret}\"; printf '%s\\n' \"{secret}\" >&2; set"
    ))?;
    assert_eq!(outcome.settled.state, CaptureState::Complete);
    assert_eq!(
        fs::read(home.path.join("project/out.txt"))?,
        secret.as_bytes(),
        "the command used the real value"
    );
    fs::remove_file(home.path.join("project/out.txt"))?;
    wait_installed(&home, &outcome)?;
    let out = home.object(outcome.payloads[0])?;
    let err = home.object(outcome.payloads[1])?;
    let out_text = String::from_utf8_lossy(&out);
    assert!(out_text.starts_with("$ASURA_SECRET_1\n"), "{out_text}");
    assert_eq!(err, b"$ASURA_SECRET_1\n");
    assert!(
        out_text
            .lines()
            .any(|line| line.contains("ASURA_SECRET_1=") && line.contains("$ASURA_SECRET_1")),
        "set shows the placeholder"
    );
    let numbers = &outcome.settled.numbers;
    assert_eq!(numbers.len(), 1, "{numbers:?}");
    assert_eq!(numbers[0].number, 1);
    assert!(outcome.settled.stdout.literal_replacements >= 1);
    assert!(outcome.settled.stderr.literal_replacements >= 1);
    assert!(!outcome.result.text.contains(&secret));
    sweep(&home.path, &[&secret])?;
    println!("PASS J1T-16/J1T-17 set and both-stream echoes store $ASURA_SECRET_1");
    Ok(())
}

/// J1T-18 (SDI-11, SDM-04): four capture workers, each fed by a real
/// process printing secrets; numbering is independent per job.
fn concurrent() -> Result<()> {
    let home = Home::new("conc")?;
    let before = max_rss();
    let secrets: Vec<String> = ["Aa1", "Bb2", "Cc3", "Dd4"]
        .iter()
        .map(|s| token(s))
        .collect();
    let jobs = secrets
        .iter()
        .map(|secret| {
            let (command, variables) = rewritten(&format!(
                "i=0; while [ $i -lt 2000 ]; do printf 'line %d token: %s\\n' $i \"{secret}\"; i=$((i+1)); done; printf 'out %s\\n' \"{secret}\""
            ));
            Job {
                command,
                secrets: variables,
            }
        })
        .collect();
    let outcomes = home.run(jobs, false, None)?;
    let after = max_rss();
    assert_eq!(outcomes.len(), 4);
    for outcome in &outcomes {
        assert_eq!(outcome.settled.state, CaptureState::Complete);
        assert_eq!(outcome.settled.numbers.len(), 1, "independent numbering");
        assert_eq!(outcome.settled.numbers[0].number, 1);
        wait_installed(&home, outcome)?;
        let stored = home.object(outcome.payloads[0])?;
        assert!(holds(&stored, "token: $ASURA_SECRET_1"));
    }
    let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
    sweep(&home.path, &refs)?;
    println!(
        "PASS J1T-18 four concurrent workers; SDM-04 process max RSS grew {} KiB ({} KiB per worker, acceptance 6 MiB each)",
        (after - before) / 1024,
        (after - before) / 4096
    );
    Ok(())
}
fn max_rss() -> u64 {
    // SAFETY: zeroed rusage is a valid output buffer.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: RUSAGE_SELF with a writable buffer.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    usage.ru_maxrss as u64
}

fn progress(home: &Home) -> u64 {
    fs::read_to_string(home.path.join("project/progress"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}
const FLOOD: &str = "i=0; while [ $i -lt 4000 ]; do printf '%0200d\\n' $i; i=$((i+1)); \
     if [ $((i % 100)) -eq 0 ]; then echo $i > progress; fi; done";

/// J1T-15 (SDI-04): a gated scanner fills the queue; pipe reads stop and
/// the producer blocks; after release nothing is lost and nothing is
/// cancelled.
fn gated_scanner() -> Result<()> {
    let home = Arc::new(Home::new("gate")?);
    fs::write(home.path.join("capture-hold-scan"), b"")?;
    let runner = {
        let home = home.clone();
        std::thread::spawn(move || {
            home.run(
                vec![Job {
                    command: FLOOD.into(),
                    secrets: Vec::new(),
                }],
                false,
                None,
            )
            .map_err(|e| e.to_string())
        })
    };
    // The producer stalls: the pipe (64 KiB) and queue (512 KiB) fill.
    let mut last = u64::MAX;
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        std::thread::sleep(Duration::from_millis(700));
        let now = progress(&home);
        if now == last && now > 0 {
            break;
        }
        last = now;
        if Instant::now() > end {
            return Err("producer never blocked".into());
        }
    }
    assert!(last < 4000, "producer blocked before finishing ({last})");
    let held = (last as usize) * 201;
    assert!(held < 2 * 1024 * 1024, "bounded by queue and pipe: {held}");
    fs::remove_file(home.path.join("capture-hold-scan"))?;
    let outcomes = runner.join().unwrap()?;
    let outcome = &outcomes[0];
    assert_eq!(outcome.settled.state, CaptureState::Complete);
    assert_eq!(outcome.result.status, 1, "not cancelled");
    assert_eq!(outcome.settled.stdout.stored_bytes, 4000 * 201);
    assert_eq!(outcome.settled.stdout.discarded_bytes, 0);
    println!(
        "PASS J1T-15 gated scanner: producer blocked at {last} lines, nothing lost, no cancel"
    );
    Ok(())
}
/// J1T-15 (SDI-05): a throttled scanner far below the producer's rate only
/// backpressures; the job settles complete with every byte.
fn throttled_scanner() -> Result<()> {
    let home = Home::new("throttle")?;
    fs::write(home.path.join("capture-throttle-scan"), b"3000")?;
    let outcome = home.one(FLOOD)?;
    assert_eq!(outcome.settled.state, CaptureState::Complete);
    assert_eq!(outcome.result.status, 1);
    assert_eq!(outcome.settled.stdout.stored_bytes, 4000 * 201);
    println!(
        "PASS J1T-15 throttled scanner: complete after {:?}",
        outcome.elapsed
    );
    Ok(())
}

/// J1T-20 (RG-02): an injected write fault stops the job and keeps the
/// prefix (`storage_fault`); a cancel while the sink is full discards and
/// counts (`discarded_after_cancel`).
fn partial_capture() -> Result<()> {
    let home = Home::new("partial")?;
    fs::write(home.path.join("payload-fault-write"), b"eio")?;
    let outcome = home.one("printf first; sleep 30")?;
    assert_eq!(outcome.settled.state, CaptureState::Incomplete);
    assert_eq!(outcome.settled.reason, CaptureReason::StorageFault);
    assert_eq!(outcome.result.status, 6, "the fault stopped the job");
    assert!(outcome.elapsed < Duration::from_secs(10));
    fs::remove_file(home.path.join("payload-fault-write"))?;
    println!("PASS J1T-20 write fault: incomplete storage_fault, job stopped");

    fs::write(home.path.join("capture-throttle-scan"), b"60000")?;
    let outcomes = home.run(
        vec![Job {
            command: "while :; do printf '%04000d\\n' 1; done".into(),
            secrets: Vec::new(),
        }],
        false,
        Some(Duration::from_millis(800)),
    )?;
    fs::remove_file(home.path.join("capture-throttle-scan"))?;
    let outcome = &outcomes[0];
    assert_eq!(outcome.result.status, 6);
    match outcome.settled.state {
        CaptureState::Incomplete => {
            assert_eq!(outcome.settled.reason, CaptureReason::DiscardedAfterCancel);
            assert!(outcome.settled.stdout.discarded_bytes > 0);
            println!(
                "PASS J1T-20 discard after cancel: {} bytes discarded and counted",
                outcome.settled.stdout.discarded_bytes
            );
        }
        other => return Err(format!("expected incomplete, got {other:?}").into()),
    }
    Ok(())
}

/// J1T-21 (RG-03, J1-Q1): a stalled scanner while running only
/// backpressures; after cancel the call settles `unconfirmed` at the L3
/// bound, the result is delivered and the worker is kept until it returns.
fn overload() -> Result<()> {
    let home = Home::new("overload")?;
    fs::write(home.path.join("capture-hold-scan"), b"")?;
    let started = Instant::now();
    let outcomes = home_run_release(&home, Duration::from_secs(3))?;
    let outcome = &outcomes[0];
    assert_eq!(outcome.settled.state, CaptureState::Unconfirmed);
    assert_eq!(outcome.settled.reason, CaptureReason::SettlementUnconfirmed);
    assert!(outcome.settled.stdout.digest.is_none());
    assert_eq!(outcome.result.status, 6);
    assert!(outcome.result.text.contains("capture=unconfirmed"));
    let elapsed = outcome.elapsed;
    assert!(elapsed >= Duration::from_millis(3000 + 2400), "{elapsed:?}");
    println!(
        "PASS J1T-21 stalled scan: running {:?} without cancel, unconfirmed {:?} after the cancel, worker kept then released ({:?} total)",
        Duration::from_secs(3),
        elapsed - Duration::from_secs(3),
        started.elapsed()
    );
    Ok(())
}
/// Runs a flood with the scanner held, cancels after `cancel`, and releases
/// the scanner once the call has settled so the kept worker can exit.
fn home_run_release(home: &Home, cancel: Duration) -> Result<Vec<Outcome>> {
    let gate = home.path.join("capture-hold-scan");
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(cancel + Duration::from_secs(4));
        let _ = fs::remove_file(gate);
    });
    let outcomes = home.run(
        vec![Job {
            command: FLOOD.into(),
            secrets: Vec::new(),
        }],
        false,
        Some(cancel),
    );
    releaser.join().unwrap();
    outcomes
}

/// J1T-23: a scanner panic writes a `detector_error` loss marker and the
/// capture continues; a second panic inside `detector_error` is a fault.
fn detector_panic() -> Result<()> {
    let home = Home::new("panic")?;
    fs::write(home.path.join("capture-panic-scan"), b"once")?;
    let outcome = home.one("printf 'one\\n'; printf 'two\\n' >&2")?;
    fs::remove_file(home.path.join("capture-panic-scan"))?;
    assert_eq!(
        outcome.settled.state,
        CaptureState::Complete,
        "{:?}",
        outcome.settled
    );
    let stream = &outcome.settled.stdout;
    assert!(stream.degraded);
    assert_eq!(stream.spans[0].reason, LossReason::DetectorError);
    wait_installed(&home, &outcome)?;
    let stored = home.object(outcome.payloads[0])?;
    assert!(
        holds(&stored, "detector_error"),
        "{}",
        String::from_utf8_lossy(&stored)
    );
    assert!(!holds(&stored, "one"), "no unchecked byte staged");
    fs::write(home.path.join("capture-panic-scan"), b"twice")?;
    let outcome = home.one("printf 'one\\n'")?;
    fs::remove_file(home.path.join("capture-panic-scan"))?;
    assert_eq!(outcome.settled.state, CaptureState::Incomplete);
    assert_eq!(outcome.settled.reason, CaptureReason::DetectorFault);
    println!("PASS J1T-23 detector panic: loss marker, then detector_fault on a second panic");
    Ok(())
}

/// J1T-09 (worker layer): free space under 16 MiB pauses capture
/// (`enospc`); restoring space resumes at the next retry delay; cancel while
/// paused records `enospc`.
fn enospc_pause() -> Result<()> {
    let home = Home::new("enospc")?;
    let free = home.path.join("payload-free-bytes");
    fs::write(&free, (16 * MIB).to_string())?;
    let releaser = {
        let free = free.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            let _ = fs::remove_file(free);
        })
    };
    let outcome = home.one("/usr/bin/yes 0123456789 | /usr/bin/head -c 3000000")?;
    releaser.join().unwrap();
    assert_eq!(outcome.settled.state, CaptureState::Complete);
    assert_eq!(outcome.settled.stdout.stored_bytes, 3_000_000);
    assert!(outcome.settled.pause_count >= 1);
    assert!(
        outcome.settled.paused_ms >= 1000,
        "{}",
        outcome.settled.paused_ms
    );
    println!(
        "PASS J1T-09 pause on low space and resume at the retry delay: {} pause(s), {} ms paused",
        outcome.settled.pause_count, outcome.settled.paused_ms
    );
    fs::write(&free, (16 * MIB).to_string())?;
    let outcomes = home.run(
        vec![Job {
            command: "/usr/bin/yes 0123456789 | /usr/bin/head -c 3000000".into(),
            secrets: Vec::new(),
        }],
        false,
        Some(Duration::from_millis(1500)),
    )?;
    fs::remove_file(&free)?;
    let outcome = &outcomes[0];
    assert_eq!(outcome.settled.state, CaptureState::Incomplete);
    assert_eq!(outcome.settled.reason, CaptureReason::Enospc);
    println!("PASS J1T-09 cancel while paused: incomplete enospc");
    Ok(())
}

/// J1T-08 (worker layer): calls in sequence fill a small quota; interim
/// expiry deletes the oldest unheld output; a page of it reads `expired`.
fn quota_expiry() -> Result<()> {
    let home = Home::new("quota")?;
    home.store.set_effective_quota(64 * MIB, u64::MAX / 2);
    let size = 40 * 1_000_000;
    let command = format!("/usr/bin/yes 0123456789 | /usr/bin/head -c {size}");
    let first = home.run(
        vec![Job {
            command: command.clone(),
            secrets: Vec::new(),
        }],
        false,
        None,
    )?;
    wait_installed(&home, &first[0])?;
    let second = home.run(
        vec![Job {
            command,
            secrets: Vec::new(),
        }],
        false,
        None,
    )?;
    assert_eq!(second[0].settled.stdout.stored_bytes, size);
    assert_eq!(second[0].settled.state, CaptureState::Complete);
    let page = read_page(
        &home.store,
        PageRequest {
            payload: first[0].payloads[0],
            offset: 0,
            limit: 100,
            scope: None,
        },
    );
    assert_eq!(page.state, PageState::Expired);
    assert!(
        !home
            .path
            .join(".asura/payloads/objects")
            .join(hex(first[0].payloads[0]))
            .exists()
    );
    assert!(home.store.used() <= 64 * MIB);
    println!(
        "PASS J1T-08 interim expiry under a 64 MiB quota: first call expired, second stored whole"
    );
    Ok(())
}

/// J1T-10 and J1T-11 integration: `live` pages stay under the watermark
/// during a flood; offsets are stable across install; a blocked read
/// answers `busy` after the 2 s step while its worker stays held.
fn pages() -> Result<()> {
    let home = Home::new("pages")?;
    let outcomes = home.run(
        vec![Job {
            command: FLOOD.into(),
            secrets: Vec::new(),
        }],
        true,
        None,
    )?;
    let outcome = &outcomes[0];
    let mut live = 0;
    for page in &outcome.live_pages {
        if page.state == PageState::Live {
            live += 1;
            assert!(page.next_offset <= page.watermark.unwrap_or(0));
            assert!(page.total <= outcome.settled.stdout.stored_bytes);
        }
    }
    assert!(live > 0);
    let before_install = read_page(
        &home.store,
        PageRequest {
            payload: outcome.payloads[0],
            offset: 4021,
            limit: 3000,
            scope: None,
        },
    );
    wait_installed(&home, outcome)?;
    let after = read_page(
        &home.store,
        PageRequest {
            payload: outcome.payloads[0],
            offset: 4021,
            limit: 3000,
            scope: None,
        },
    );
    assert_eq!(before_install.text, after.text);
    assert_eq!(after.state, PageState::Complete);
    assert_eq!(after.digest, outcome.settled.stdout.digest);
    let pool = PagePool::start(home.store.clone())?;
    pool.hold_for_test(true);
    let request = PageRequest {
        payload: outcome.payloads[0],
        offset: 0,
        limit: 10,
        scope: None,
    };
    let held = pool
        .submit(request, Origin::Client)
        .map_err(|s| s.as_str())?;
    let started = Instant::now();
    let page = held.wait();
    assert_eq!(page.state, PageState::Busy);
    assert!(started.elapsed() >= Duration::from_millis(1900));
    // The general worker is still held; the reserved worker serves a model read.
    let model = pool
        .submit(request, Origin::Model)
        .map_err(|s| s.as_str())?;
    let page = model.wait();
    assert_eq!(
        page.state,
        PageState::Busy,
        "both workers held by the test hold"
    );
    pool.hold_for_test(false);
    let page = pool
        .submit(request, Origin::Model)
        .map_err(|s| s.as_str())?
        .wait();
    assert_eq!(page.state, PageState::Complete);
    println!(
        "PASS J1T-10/J1T-11 live pages under the watermark ({live}), stable offsets, busy after the 2 s step"
    );
    Ok(())
}

/// A scratch APFS disk image, detached and deleted on drop.
struct DiskImage {
    image: PathBuf,
    mount: PathBuf,
}
impl DiskImage {
    fn new(megabytes: u32) -> Result<Self> {
        let name: String = asura_platform::random_id()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let base = PathBuf::from("/private/tmp").join(format!("as-cw-disk-{}", &name[..12]));
        fs::create_dir(&base)?;
        let image = base.join("j1.dmg");
        let mount = base.join("mnt");
        fs::create_dir(&mount)?;
        let created = std::process::Command::new("/usr/bin/hdiutil")
            .args(["create", "-quiet", "-size"])
            .arg(format!("{megabytes}m"))
            .args(["-fs", "APFS", "-volname", "asura-j1"])
            .arg(&image)
            .status()?;
        if !created.success() {
            return Err("hdiutil create".into());
        }
        let attached = std::process::Command::new("/usr/bin/hdiutil")
            .args(["attach", "-quiet", "-nobrowse", "-noverify", "-mountpoint"])
            .arg(&mount)
            .arg(&image)
            .status()?;
        if !attached.success() {
            return Err("hdiutil attach".into());
        }
        Ok(Self { image, mount })
    }
}
impl Drop for DiskImage {
    fn drop(&mut self) {
        let _ = std::process::Command::new("/usr/bin/hdiutil")
            .args(["detach", "-quiet", "-force"])
            .arg(&self.mount)
            .status();
        if let Some(base) = self.image.parent() {
            let _ = fs::remove_dir_all(base);
        }
    }
}
fn fill(path: &Path, leave: u64, store: &PayloadStore) -> Result<()> {
    let free = store.free_bytes().map_err(|e| format!("{e:?}"))?;
    let mut file = fs::File::create(path)?;
    let block = vec![0u8; 1 << 20];
    let mut remaining = free.saturating_sub(leave);
    while remaining > 0 {
        let n = remaining.min(block.len() as u64) as usize;
        std::io::Write::write_all(&mut file, &block[..n])?;
        remaining -= n as u64;
    }
    file.sync_all()?;
    Ok(())
}

/// J1T-09 (constrained APFS disk image): free space under the 16 MiB
/// recovery reserve pauses capture; freeing space outside Asura resumes it
/// at the next delay; a space event resumes it at once; cancel while
/// paused records `enospc`.
fn disk_image() -> Result<()> {
    let disk = DiskImage::new(160)?;
    let home = Home::at(&disk.mount, "disk")?;
    let filler = disk.mount.join("filler");
    let command = "/usr/bin/yes 0123456789 | /usr/bin/head -c 12000000";
    // 1. Freed outside Asura: resumes at the next retry delay.
    fill(&filler, 22 * MIB, &home.store)?;
    let releaser = {
        let filler = filler.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            let _ = fs::remove_file(filler);
        })
    };
    let outcome = home.one(command)?;
    releaser.join().unwrap();
    assert_eq!(outcome.settled.state, CaptureState::Complete);
    assert_eq!(outcome.settled.stdout.stored_bytes, 12_000_000);
    assert!(
        outcome.settled.pause_count >= 1,
        "paused on the real volume"
    );
    let delay_resume = outcome.settled.paused_ms;
    assert!(delay_resume >= 1000, "{delay_resume}");
    // 2. A space event (here, an expiry or settlement) resumes at once.
    fill(&filler, 22 * MIB, &home.store)?;
    let releaser = {
        let filler = filler.clone();
        let store = home.store.clone();
        let staging = home.path.join(".asura/payloads/staging");
        std::thread::spawn(move || {
            // Wait until the capture has paused (staging stops growing), then
            // free space between the 1 s and 2 s retries and signal it.
            let size = || -> u64 {
                fs::read_dir(&staging)
                    .map(|d| {
                        d.flatten()
                            .filter_map(|e| e.metadata().ok())
                            .map(|m| m.len())
                            .sum()
                    })
                    .unwrap_or(0)
            };
            let mut last = u64::MAX;
            loop {
                std::thread::sleep(Duration::from_millis(200));
                let now = size();
                if now == last && now > 0 {
                    break;
                }
                last = now;
            }
            std::thread::sleep(Duration::from_millis(1100));
            let _ = fs::remove_file(filler);
            store.space_event();
        })
    };
    let outcome = home.one(command)?;
    releaser.join().unwrap();
    assert_eq!(outcome.settled.state, CaptureState::Complete);
    let event_resume = outcome.settled.paused_ms;
    // Retries fall at 1 s and 2 s of pause; the event came at about 1.3 s.
    assert!(
        (1200..1950).contains(&event_resume),
        "resumed on the event, not at a retry: {event_resume}"
    );
    // 3. Cancel while paused.
    fill(&filler, 22 * MIB, &home.store)?;
    let outcomes = home.run(
        vec![Job {
            command: command.into(),
            secrets: Vec::new(),
        }],
        false,
        Some(Duration::from_millis(2500)),
    )?;
    let _ = fs::remove_file(&filler);
    assert_eq!(outcomes[0].settled.state, CaptureState::Incomplete);
    assert_eq!(outcomes[0].settled.reason, CaptureReason::Enospc);
    println!(
        "PASS J1T-09 disk image: paused under the reserve, resumed at the delay ({delay_resume} ms) and on a space event ({event_resume} ms), cancel while paused recorded enospc"
    );
    drop(home);
    Ok(())
}

type Case = (&'static str, fn() -> Result<()>);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let [_, flag, id, deadline, command] = args.as_slice()
        && flag == "--asura-shell-guardian"
    {
        std::process::exit(asura_platform::shell::run_guardian(id, deadline, command));
    }
    let selected: Vec<&str> = args[1..]
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let cases: [Case; 14] = [
        ("full", full_capture),
        ("split", split_secret),
        ("klb", key_long_binary),
        ("cmd", command_secrets),
        ("conc", concurrent),
        ("gate", gated_scanner),
        ("throttle", throttled_scanner),
        ("partial", partial_capture),
        ("overload", overload),
        ("panic", detector_panic),
        ("enospc", enospc_pause),
        ("quota", quota_expiry),
        ("pages", pages),
        ("disk", disk_image),
    ];
    for (name, case) in cases {
        if !selected.is_empty() && !selected.contains(&name) {
            continue;
        }
        if let Err(error) = case() {
            eprintln!("capture worker case {name} FAILED: {error}");
            std::process::exit(1);
        }
    }
    println!("capture worker qualification passed");
}
