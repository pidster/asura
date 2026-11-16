//! Solver process owner tests (security-policy.md, PL3 validation map).
//! Real processes, real pipes and the real solver profile, in scratch
//! runtimes under `/private/tmp`; never the account's Asura home.
use super::*;
use std::os::unix::fs::PermissionsExt;

struct Scratch {
    root: PathBuf,
    runtime: RuntimeDirectory,
}
impl Scratch {
    fn new() -> Self {
        let name: String = crate::random_id()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let root = PathBuf::from(format!("/private/tmp/asura-solver-{}", &name[..12]));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = RuntimeDirectory::scratch(&root, true).unwrap();
        Self { root, runtime }
    }
    fn copies(&self) -> usize {
        std::fs::read_dir(self.root.join(".asura/run"))
            .unwrap()
            .filter_map(|v| v.ok())
            .filter(|v| v.file_name().to_string_lossy().starts_with("solver-"))
            .count()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn write_manifest(dir: &Path, text: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("manifest.json"), text).unwrap();
}

/// The manifest form of `tools/release/solver/build`: licence entries are
/// skipped; bad paths, digests, sizes and versions refuse.
#[test]
fn manifest_reads_the_build_form_and_refuses_malformed_ones() {
    let scratch = Scratch::new();
    let dir = scratch.root.join("package");
    let digest = "ab".repeat(32);
    write_manifest(
        &dir,
        &format!(
            r#"{{"format_version": 1, "lock_sha256": "{digest}", "files": {{
            "Frameworks/libgmp.10.dylib": {{"bytes": 3, "sha256": "{digest}"}},
            "MacOS/cvc5": {{"bytes": 5, "sha256": "{digest}"}},
            "licenses/gmp-COPYINGv3.txt": {{"bytes": 7, "sha256": "{digest}"}}}}}}"#
        ),
    );
    let package = SolverPackage::read(&dir).unwrap();
    assert_eq!(package.files.len(), 2);
    assert_eq!(package.files[1].path, "MacOS/cvc5");
    assert_eq!(package.files[1].sha256, [0xab; 32]);
    assert_ne!(package.digest(), [0; 32]);
    for bad in [
        r#"{"format_version": 2, "files": {"MacOS/cvc5": {"bytes": 1, "sha256": "SHA"}}}"#
            .to_owned(),
        format!(
            r#"{{"format_version": 1, "files": {{"MacOS/other": {{"bytes": 1, "sha256": "{digest}"}}}}}}"#
        ),
        format!(
            r#"{{"format_version": 1, "files": {{"MacOS/cvc5": {{"bytes": 0, "sha256": "{digest}"}}}}}}"#
        ),
        r#"{"format_version": 1, "files": {"MacOS/cvc5": {"bytes": 1, "sha256": "AB"}}}"#
            .to_owned(),
        format!(
            r#"{{"format_version": 1, "files": {{"MacOS/cvc5": {{"bytes": 1, "sha256": "{digest}"}}, "../x": {{"bytes": 1, "sha256": "{digest}"}}}}}}"#
        ),
        "not json".to_owned(),
    ] {
        write_manifest(&dir, &bad);
        assert!(
            matches!(SolverPackage::read(&dir), Err(SolverError::Manifest)),
            "{bad}"
        );
    }
    std::fs::remove_file(dir.join("manifest.json")).unwrap();
    assert!(matches!(
        SolverPackage::read(&dir),
        Err(SolverError::Manifest)
    ));
}

#[test]
fn profile_quotes_the_copy_directory() {
    let text = sandbox_profile("/private/tmp/a \"b\"").unwrap();
    assert!(text.starts_with("(version 1)(deny default)"));
    assert!(text.contains(r#"(literal "/private/tmp/a \"b\"/MacOS/cvc5")"#));
    assert!(text.contains(r#"(subpath "/private/tmp/a \"b\"")"#));
    assert!(sandbox_profile("/tmp/a\nb").is_err());
}

#[cfg(feature = "test-support")]
mod process {
    use super::*;
    use std::time::Instant;

    fn fixture(scratch: &Scratch, mode: &str) -> SolverPackage {
        let exe = fixture_executable()
            .expect("solver_fixture missing: run cargo test with --features test-support");
        let dir = scratch.root.join(format!("fixture-{mode}"));
        fixture_package(&exe, mode, &dir).unwrap();
        SolverPackage::read(&dir).unwrap()
    }
    fn start(scratch: &Scratch, package: &SolverPackage, cancel: &SolverCancel) -> SolverProcess {
        SolverProcess::start(&scratch.runtime, package, SolverLimits::default(), cancel).unwrap()
    }
    /// A one-line answer reader for the fixture answers.
    fn line_reader(out: &mut Vec<u8>) -> impl FnMut(Stream, &[u8]) -> Flow + '_ {
        move |stream, bytes| {
            if stream == Stream::Stdout {
                out.extend_from_slice(bytes);
                if out.contains(&b'\n') {
                    return Flow::Answered;
                }
            }
            Flow::More
        }
    }

    /// A query is answered through the copy, which is private and removed
    /// after the normal finish; the process exits at end of input.
    #[test]
    fn answers_through_the_verified_copy_and_cleans_up() {
        let scratch = Scratch::new();
        let package = fixture(&scratch, "answer-unsat");
        let cancel = SolverCancel::new().unwrap();
        let mut solver = start(&scratch, &package, &cancel);
        let copy = solver.copy_directory().to_path_buf();
        assert!(copy.join("MacOS/cvc5").is_file());
        assert!(copy.join("Frameworks/fixture.mode").is_file());
        assert_eq!(scratch.copies(), 1);
        for _ in 0..3 {
            let mut out = Vec::new();
            solver
                .exchange(b"(assert true)\n(check-sat)\n", &mut line_reader(&mut out))
                .unwrap();
            assert_eq!(out, b"unsat\n");
        }
        let exit = solver.finish();
        assert_eq!(exit.status, Some(0));
        assert!(exit.peak_footprint > 0);
        assert!(!copy.exists());
        assert_eq!(scratch.copies(), 0);
    }

    /// A package file whose digest differs is refused before any spawn,
    /// and no copy is left.
    #[test]
    fn digest_mismatch_refuses_before_spawn() {
        let scratch = Scratch::new();
        let package = fixture(&scratch, "answer-unsat");
        std::fs::write(package.directory.join("Frameworks/fixture.mode"), "silent").unwrap();
        let cancel = SolverCancel::new().unwrap();
        let error =
            SolverProcess::start(&scratch.runtime, &package, SolverLimits::default(), &cancel)
                .err()
                .unwrap();
        assert_eq!(error.cause(), "digest_mismatch");
        assert_eq!(scratch.copies(), 0);
        cancel.cancel();
        let error =
            SolverProcess::start(&scratch.runtime, &package, SolverLimits::default(), &cancel)
                .err()
                .unwrap();
        assert_eq!(error.cause(), "cancelled");
    }

    /// SA-T1: the fixture reads and never answers; the exchange waits until
    /// a cancel from another thread; the kill reaps it and removes the copy.
    #[test]
    fn sa_t1_stalled_solver_waits_until_cancel() {
        let scratch = Scratch::new();
        let package = fixture(&scratch, "silent");
        let cancel = SolverCancel::new().unwrap();
        let mut solver = start(&scratch, &package, &cancel);
        let pid = solver.pid();
        let canceller = cancel.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            canceller.cancel();
        });
        let started = Instant::now();
        let mut out = Vec::new();
        let end = solver.exchange(b"(check-sat)\n", &mut line_reader(&mut out));
        assert!(matches!(end, Err(ExchangeEnd::Cancelled)), "{end:?}");
        assert!(started.elapsed() >= Duration::from_millis(350));
        thread.join().unwrap();
        let exit = solver.kill();
        assert_eq!(exit.status, None, "killed by a signal");
        // SAFETY: probe only; the reaped PID no longer names our child.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(scratch.copies(), 0);
    }

    /// SA-T2: the fixture writes 1 MiB to standard output before it reads;
    /// a 4 MiB query is still written completely because the loop drains.
    #[test]
    fn sa_t2_output_first_does_not_stall_the_write() {
        let scratch = Scratch::new();
        let package = fixture(&scratch, "flood-stdout");
        let cancel = SolverCancel::new().unwrap();
        let mut solver = start(&scratch, &package, &cancel);
        let query = vec![b' '; 4 * 1024 * 1024];
        let mut seen = 0usize;
        let mut first: Vec<u8> = Vec::new();
        let result = solver.exchange(&query, &mut |stream, bytes| {
            if stream == Stream::Stdout {
                seen += bytes.len();
                if first.len() < 64 {
                    first.extend_from_slice(&bytes[..bytes.len().min(64)]);
                }
                // The first line is the "answer"; the rest is drained.
                return Flow::Answered;
            }
            Flow::More
        });
        assert!(result.is_ok(), "{result:?}");
        assert!(seen > 0 && first.starts_with(b"zzz"));
        solver.kill();
    }

    /// SA-T3: 1 MiB on standard error first; the query completes with the
    /// fixture's answer, and the stream is drained.
    #[test]
    fn sa_t3_standard_error_flood_is_drained() {
        let scratch = Scratch::new();
        let package = fixture(&scratch, "flood-stderr");
        let cancel = SolverCancel::new().unwrap();
        let mut solver = start(&scratch, &package, &cancel);
        let mut query = vec![b' '; 4 * 1024 * 1024];
        query.extend_from_slice(b"(check-sat)\n");
        let mut out = Vec::new();
        let mut errors = 0usize;
        solver
            .exchange(&query, &mut |stream, bytes| match stream {
                Stream::Stderr => {
                    errors += bytes.len();
                    Flow::More
                }
                Stream::Stdout => {
                    out.extend_from_slice(bytes);
                    if out.contains(&b'\n') {
                        Flow::Answered
                    } else {
                        Flow::More
                    }
                }
            })
            .unwrap();
        assert_eq!(out, b"unsat\n");
        assert!(errors > 0);
        solver.finish();
    }

    /// SA-T4: a model line over 1 MiB: the closure faults at its bound and
    /// the exchange returns at once; the kill reaps it.
    #[test]
    fn sa_t4_model_over_the_bound_faults() {
        let scratch = Scratch::new();
        let package = fixture(&scratch, "huge-model");
        let cancel = SolverCancel::new().unwrap();
        let mut solver = start(&scratch, &package, &cancel);
        let mut out = Vec::new();
        solver
            .exchange(b"(check-sat)\n", &mut line_reader(&mut out))
            .unwrap();
        assert_eq!(out, b"sat\n");
        let mut kept = 0usize;
        let end = solver.exchange(b"(get-model)\n", &mut |stream, bytes| {
            if stream == Stream::Stdout {
                kept += bytes.len();
                if kept > 1024 * 1024 {
                    return Flow::Fault;
                }
            }
            Flow::More
        });
        assert!(matches!(end, Err(ExchangeEnd::Fault)), "{end:?}");
        assert!(kept <= 1024 * 1024 + 64 * 1024);
        let exit = solver.kill();
        assert_eq!(exit.status, None);
    }

    /// SA-T5: the fixture exits after 2 MiB of a 4 MiB query: the broken
    /// pipe or end of output ends the exchange `Closed`; the status is 3.
    #[test]
    fn sa_t5_exit_mid_query_is_closed_with_status() {
        let scratch = Scratch::new();
        let package = fixture(&scratch, "exit-half");
        let cancel = SolverCancel::new().unwrap();
        let mut solver = start(&scratch, &package, &cancel);
        let query = vec![b' '; 4 * 1024 * 1024];
        let mut out = Vec::new();
        let end = solver.exchange(&query, &mut line_reader(&mut out));
        assert!(matches!(end, Err(ExchangeEnd::Closed)), "{end:?}");
        let exit = solver.finish();
        assert_eq!(exit.status, Some(3));
    }

    /// PL3 solver memory bound: a fixture that allocates past a lowered
    /// bound is killed and reaped; the overshoot is printed.
    #[test]
    fn memory_bound_kills_the_solver() {
        let scratch = Scratch::new();
        let package = fixture(&scratch, "memory");
        let cancel = SolverCancel::new().unwrap();
        let limits = SolverLimits {
            memory_bytes: 256 * 1024 * 1024,
        };
        let mut solver = SolverProcess::start(&scratch.runtime, &package, limits, &cancel).unwrap();
        let mut out = Vec::new();
        let end = solver.exchange(b"(check-sat)\n", &mut line_reader(&mut out));
        assert!(matches!(end, Err(ExchangeEnd::MemoryBound)), "{end:?}");
        let exit = solver.kill();
        assert!(exit.memory_bound && exit.status.is_none());
        assert!(exit.peak_footprint > limits.memory_bytes);
        println!(
            "memory bound 256 MiB: peak sampled footprint {} bytes, overshoot {} bytes",
            exit.peak_footprint,
            exit.peak_footprint - limits.memory_bytes
        );
        assert_eq!(scratch.copies(), 0);
    }

    /// Dropping an unsettled process kills, reaps and removes the copy.
    #[test]
    fn drop_settles_the_process() {
        let scratch = Scratch::new();
        let package = fixture(&scratch, "silent");
        let cancel = SolverCancel::new().unwrap();
        let solver = start(&scratch, &package, &cancel);
        let pid = solver.pid();
        drop(solver);
        // SAFETY: probe only.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(scratch.copies(), 0);
    }

    /// PL-I6 (platform part): the development cvc5 package in the Cargo
    /// profile directory answers `sat` under the solver profile. An absent
    /// package fails the test: a gap, never a pass.
    #[test]
    fn real_cvc5_answers_under_the_profile() {
        let profile = cargo_profile_directory(&std::env::current_exe().unwrap()).unwrap();
        let dir = profile.join("solver");
        let package = SolverPackage::read(&dir).unwrap_or_else(|e| {
            panic!(
                "{}: {e}: copy the tools/release/solver/build package there",
                dir.display()
            )
        });
        assert_eq!(package.files.len(), 2);
        let scratch = Scratch::new();
        let cancel = SolverCancel::new().unwrap();
        let mut solver = start(&scratch, &package, &cancel);
        let mut out = Vec::new();
        solver
            .exchange(
                b"(set-logic ALL)\n(declare-const x Int)\n(assert (> (* x x) 4))\n(check-sat)\n",
                &mut line_reader(&mut out),
            )
            .unwrap();
        assert_eq!(out, b"sat\n");
        let exit = solver.finish();
        assert_eq!(exit.status, Some(0));
        println!("cvc5 peak footprint {} bytes", exit.peak_footprint);
    }
}
