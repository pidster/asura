//! Path guard unit tests: EPG-U1 (list matching and the walk), EPG-U2
//! (fingerprint differences), EPG-U4 (warning computation), and the
//! worker's admission, cancellation and digest rules. Each case uses its
//! own scratch directory under `/private/tmp`.
use super::check::{self, Facts, Warning, Warnings};
use super::lists::{self, List};
use super::walk::{self, FileState};
use super::worker::{self, Guard, Polled, Refused, Reply, Root, Work};
use super::*;
use asura_storage::authority::conversation::{
    Locator, PathAction, PathApproval, PathChange, PathEntryKind as K, PathGuardView,
};
use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        use std::os::unix::fs::DirBuilderExt;
        let path = PathBuf::from(format!(
            "/private/tmp/as-pg-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        std::fs::create_dir(path.join("project")).unwrap();
        Self(path)
    }
    fn root(&self) -> PathBuf {
        self.0.join("project")
    }
    fn file(&self, relative: &str, content: &[u8]) {
        let path = self.root().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn keys(walk: &walk::Walk) -> Vec<&str> {
    walk.paths.keys().map(String::as_str).collect()
}
fn run(root: &Path, list: List) -> walk::Walk {
    walk::walk(root, list, &AtomicBool::new(false)).unwrap()
}

/// EPG-U1: the predicates at and past each depth bound.
#[test]
fn epg_u1_list_predicates_and_depths() {
    for yes in [
        ".git/hooks/pre-commit",
        ".git/hooks/sub/x",
        ".git/config",
        "a/b/c/d/e/f/g/h/.git/config",
        "a/b/c/d/e/f/g/h/.git/hooks/x",
        ".gitattributes",
        "a/.gitmodules",
        "a/b/c/d/e/f/g/h/.envrc",
        ".cargo/config",
        "a/b/c/d/e/f/g/h/.cargo/config.toml",
    ] {
        assert!(lists::implicit(yes), "{yes}");
    }
    for no in [
        "a/b/c/d/e/f/g/h/i/.envrc",
        "a/b/c/d/e/f/g/h/i/.git/config",
        ".git/hooks",
        ".git/HEAD",
        ".git/objects/x",
        "target/.envrc",
        "node_modules/x/.envrc",
        "build/.cargo/config",
        ".cargo/credentials",
        "envrc",
        "/.envrc",
        "a/../.envrc",
        "./.envrc",
    ] {
        assert!(!lists::implicit(no), "{no}");
    }
    for yes in [
        "build.rs",
        "a/b/c/d/Cargo.toml",
        "x.mk",
        "cmake/x.cmake",
        "jest.config.js",
        "vitest.config.mts",
        "conftest.py",
        "pkg/foo_test.go",
        "test_x.py",
        "x_test.py",
        "a.test.ts",
        "a.spec.js",
        "tests/a.rs",
        "tests/a/b/c/d.rs",
        "a/b/c/d/tests/e/f/g/h.rs",
        "a/__tests__/x",
        "spec/x",
    ] {
        assert!(lists::build_or_test(yes), "{yes}");
    }
    for no in [
        "a/b/c/d/e/Cargo.toml",
        "tests/a/b/c/d/e.rs",
        "a/b/c/d/e/tests/f.rs",
        "target/build.rs",
        "build/Cargo.toml",
        ".git/Makefile",
        "node_modules/x/package.json",
        "src/main.rs",
        ".mk",
        "test_.py",
        "jest.config.",
    ] {
        assert!(!lists::build_or_test(no), "{no}");
    }
    // Any test directory within the bound counts: the second one here.
    assert!(lists::build_or_test("tests/b/c/tests/e/f/g/h.rs"));
    assert!(!lists::build_or_test("a/tests/b/c/d/tests/e/f/g.rs"));
    // The lists name the guardian's deny patterns (EPG-P).
    for file in lists::IMPLICIT_FILES {
        assert!(lists::implicit(file), "{file}");
    }
    assert!(lists::implicit(&format!("{}/x", lists::IMPLICIT_HOOKS)));
}

/// EPG-U1: the walk over a fixture tree with nested repositories, skipped
/// directories, symlinks and depth limits gives exact path sets.
#[test]
fn epg_u1_walk_exact_sets() {
    let s = Scratch::new();
    for path in [
        ".git/config",
        ".git/HEAD",
        ".git/hooks/pre-commit",
        ".git/hooks/nested/x",
        ".git/objects/ab/cd",
        ".envrc",
        "sub/.git/config",
        "sub/.git/hooks/post-merge",
        "sub/.gitmodules",
        "a/b/c/d/e/f/g/h/.envrc",
        "a/b/c/d/e/f/g/h/i/.envrc",
        "target/.envrc",
        "node_modules/m/.envrc",
        "dist/.cargo/config",
        ".cargo/config.toml",
        "build.rs",
        "Cargo.toml",
        "crates/x/Cargo.toml",
        "crates/x/tests/flow.rs",
        "a/b/c/d/e/Cargo.toml",
        "target/debug/build.rs",
        "src/main.rs",
    ] {
        s.file(path, b"x");
    }
    let root = s.root();
    // Symlinks: a linked directory is never entered; a linked listed file
    // is listed as the link.
    symlink("/", root.join("rootlink")).unwrap();
    symlink(root.join("crates"), root.join("crates-link")).unwrap();
    symlink("/etc/hosts", root.join(".gitattributes")).unwrap();
    let implicit = run(&root, List::Implicit);
    assert_eq!(
        keys(&implicit),
        [
            ".cargo/config.toml",
            ".envrc",
            ".git/config",
            ".git/hooks/nested/x",
            ".git/hooks/pre-commit",
            ".gitattributes",
            "a/b/c/d/e/f/g/h/.envrc",
            "sub/.git/config",
            "sub/.git/hooks/post-merge",
            "sub/.gitmodules",
        ]
    );
    assert!(!implicit.overflow);
    let link = &implicit.paths[".gitattributes"];
    assert_eq!(link.kind, 0o120_000, "the link itself, not /etc/hosts");
    let build = run(&root, List::BuildTest);
    assert_eq!(
        keys(&build),
        [
            "Cargo.toml",
            "build.rs",
            "crates/x/Cargo.toml",
            "crates/x/tests/flow.rs"
        ]
    );
    assert!(!build.overflow);
}

/// EPG-U1, bounds: past 1,024 build and test paths the first 1,024 in byte
/// order are kept and the walk reports overflow; past 20,000 entries too.
#[test]
fn epg_u1_walk_bounds() {
    let s = Scratch::new();
    let root = s.root();
    std::fs::create_dir(root.join("tests")).unwrap();
    for i in 0..1_100 {
        std::fs::write(root.join(format!("tests/t{i:04}.rs")), b"").unwrap();
    }
    let build = run(&root, List::BuildTest);
    assert!(build.overflow);
    assert_eq!(build.paths.len(), lists::MAX_BUILD_PATHS);
    assert_eq!(keys(&build)[0], "tests/t0000.rs");
    assert_eq!(keys(&build)[1_023], "tests/t1023.rs");
    // 20,001 entries in one directory: the walk stops at its entry bound.
    let s = Scratch::new();
    let root = s.root();
    std::fs::create_dir(root.join("many")).unwrap();
    for i in 0..lists::MAX_ENTRIES {
        std::fs::write(root.join(format!("many/{i:05}")), b"").unwrap();
    }
    std::fs::write(root.join("zz.envrc-not"), b"").unwrap();
    let implicit = run(&root, List::Implicit);
    assert!(implicit.overflow);
    // Cancelled walks give no result.
    let cancelled = walk::walk(&root, List::Implicit, &AtomicBool::new(true));
    assert_eq!(cancelled, Err(walk::WalkError::Cancelled));
    assert_eq!(
        walk::walk(&root.join("absent"), List::Implicit, &AtomicBool::new(false)),
        Err(walk::WalkError::Unavailable)
    );
}

/// EPG-U2: add, remove, change size, change mtime and replace inode each
/// list the path once.
#[test]
fn epg_u2_fingerprint_differences() {
    let s = Scratch::new();
    let root = s.root();
    for path in ["size.rs", "mtime.rs", "inode.rs", "gone.rs", "same.rs"] {
        s.file(&format!("tests/{path}"), b"one");
    }
    let before = run(&root, List::BuildTest);
    s.file("tests/size.rs", b"longer");
    let mtime = root.join("tests/mtime.rs");
    let old = std::fs::metadata(&mtime).unwrap();
    let file = std::fs::File::options().write(true).open(&mtime).unwrap();
    file.set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1))
        .unwrap();
    drop(file);
    assert_eq!(std::fs::metadata(&mtime).unwrap().size(), old.size());
    let inode = root.join("tests/inode.rs");
    let ino = std::fs::metadata(&inode).unwrap().ino();
    std::fs::write(root.join("tests/inode.tmp"), b"one").unwrap();
    std::fs::rename(root.join("tests/inode.tmp"), &inode).unwrap();
    assert_ne!(std::fs::metadata(&inode).unwrap().ino(), ino);
    std::fs::remove_file(root.join("tests/gone.rs")).unwrap();
    s.file("tests/new.rs", b"one");
    let after = run(&root, List::BuildTest);
    assert_eq!(
        walk::changed(&before, &after),
        [
            "tests/gone.rs",
            "tests/inode.rs",
            "tests/mtime.rs",
            "tests/new.rs",
            "tests/size.rs"
        ]
    );
    assert!(walk::changed(&after, &after).is_empty());
    // The change set records them; no change, no record.
    let change = model_change(&Ok(before.clone()), &Ok(after.clone())).unwrap();
    assert_eq!(change.paths.len(), 5);
    assert_eq!(change.overflow, 0);
    assert_eq!(model_change(&Ok(after.clone()), &Ok(after.clone())), None);
    // A failed snapshot counts one unlisted change.
    assert_eq!(
        model_change(&Err(walk::WalkError::Unavailable), &Ok(after.clone())),
        Some(ModelChange {
            paths: Vec::new(),
            overflow: 1
        })
    );
    // A snapshot past its bound counts one, with no listed change.
    let mut bounded = after.clone();
    bounded.overflow = true;
    assert_eq!(
        model_change(&Ok(after.clone()), &Ok(bounded)),
        Some(ModelChange {
            paths: Vec::new(),
            overflow: 1
        })
    );
    // More than 16 changes: 16 listed in byte order, the rest counted.
    for i in 0..20 {
        s.file(&format!("tests/m{i:02}.rs"), b"");
    }
    let more = run(&root, List::BuildTest);
    let change = model_change(&Ok(after), &Ok(more)).unwrap();
    assert_eq!(change.paths.len(), 16);
    assert_eq!(change.paths[0], "tests/m00.rs");
    assert_eq!(change.overflow, 4);
}

fn frame(sequence: u64) -> Locator {
    Locator {
        sequence,
        segment: 1,
        offset: 0,
        length: 1,
    }
}
fn change(sequence: u64, paths: &[&str], overflow: u32) -> PathChange {
    PathChange {
        frame: frame(sequence),
        job: [sequence as u8; 16],
        paths: paths.iter().map(|p| p.to_string()).collect(),
        overflow,
    }
}
fn approval(path: &str, digest: u8) -> PathApproval {
    PathApproval {
        path: path.into(),
        digest: [digest; 32],
        frame: frame(1),
    }
}
fn warning(kind: K, path: &str) -> Warning {
    Warning {
        kind,
        path: path.into(),
    }
}

/// EPG-U4: each kind computed from fixture records and digests; kind 1
/// cleared by an acknowledgement; kinds 2 and 3 repeat.
#[test]
fn epg_u4_warning_computation() {
    let view = PathGuardView {
        changes: vec![
            change(10, &["build.rs", "package.json"], 0),
            change(12, &["build.rs"], 0),
        ],
        approvals: vec![
            approval(".envrc", 1),
            approval(".git/config", 2),
            approval(".git/hooks/gone", 3),
            approval(".git/hooks/big", 4),
        ],
        ..Default::default()
    };
    let facts = Facts {
        files: BTreeMap::from([
            (".envrc".into(), FileState::Digest([1; 32])),
            (".git/config".into(), FileState::Digest([9; 32])),
            (".git/hooks/pre-commit".into(), FileState::Digest([5; 32])),
            (".git/hooks/huge".into(), FileState::TooLarge),
            (".git/hooks/odd".into(), FileState::Unreadable),
            (".git/hooks/big".into(), FileState::TooLarge),
        ]),
        approved: BTreeMap::from([(".git/hooks/gone".into(), FileState::Missing)]),
        overflow: false,
    };
    let all = check::all(&view, &facts);
    assert_eq!(
        all,
        [
            warning(K::ModelChanged, "build.rs"),
            warning(K::ModelChanged, "package.json"),
            warning(K::NotApproved, ".git/hooks/odd"),
            warning(K::NotApproved, ".git/hooks/pre-commit"),
            warning(K::ChangedSinceApproval, ".git/config"),
            warning(K::ChangedSinceApproval, ".git/hooks/gone"),
            warning(K::TooLarge, ".git/hooks/big"),
            warning(K::TooLarge, ".git/hooks/huge"),
        ]
    );
    assert_eq!(check::model_through(&view), 12);
    assert_eq!(view.through(), 12);
    let warnings = Warnings::from_all(all);
    assert!(warnings.model_changed());
    // The acknowledgement covers through 12; the view after it has no
    // pending change, so kind 1 is gone and kinds 2-4 repeat.
    let ack = check::acknowledgement([7; 16], [8; 16], &view, &warnings);
    assert_eq!(ack.action, PathAction::Acknowledged);
    assert_eq!(ack.through, 12);
    assert_eq!(ack.entries.len(), 8);
    ack.validate().unwrap();
    let after = PathGuardView {
        changes: Vec::new(),
        ..view.clone()
    };
    let again = check::all(&after, &facts);
    assert!(again.iter().all(|w| w.kind != K::ModelChanged));
    assert_eq!(again.len(), 6);
    assert!(!Warnings::from_all(again).model_changed());
    // `approve all`: not approved and changed files with a digest.
    assert_eq!(
        check::approvable(&view, &facts),
        [
            (".git/config".to_owned(), [9; 32]),
            (".git/hooks/pre-commit".to_owned(), [5; 32]),
        ]
    );
    // Overflow: a walk bound, a change past its bound, or omitted changes.
    for (view, facts) in [
        (
            PathGuardView::default(),
            Facts {
                overflow: true,
                ..Facts::default()
            },
        ),
        (
            PathGuardView {
                changes: vec![change(3, &[], 1)],
                ..Default::default()
            },
            Facts::default(),
        ),
        (
            PathGuardView {
                changes: vec![change(3, &["a/Makefile"], 0)],
                omitted_changes: 2,
                ..Default::default()
            },
            Facts::default(),
        ),
    ] {
        let all = check::all(&view, &facts);
        assert_eq!(all.last(), Some(&warning(K::Overflow, check::OVERFLOW_PATH)));
    }
    // An overflow-only change holds nothing: no kind 1, through 0.
    let only = PathGuardView {
        changes: vec![change(3, &[], 1)],
        ..Default::default()
    };
    assert_eq!(check::model_through(&only), 0);
    assert!(!Warnings::from_all(check::all(&only, &Facts::default())).model_changed());
    // 20 warnings: 16 listed, 4 counted.
    let many = PathGuardView {
        changes: vec![change(
            5,
            &(0..20).map(|i| format!("t{i:02}")).collect::<Vec<_>>().iter().map(String::as_str).collect::<Vec<_>>(),
            0,
        )],
        ..Default::default()
    };
    let listed = Warnings::from_all(check::all(&many, &Facts::default()));
    assert_eq!(listed.listed.len(), 16);
    assert_eq!(listed.overflow, 4);
    let record = check::acknowledgement([7; 16], [8; 16], &many, &listed);
    assert_eq!(record.overflow, 4);
    record.validate().unwrap();
    assert_eq!(Warnings::from_record(&record), listed);
}

/// Digests: a regular file, a symlink (its target name), a FIFO (never
/// opened), a file over 1 MiB, a missing file and a linked parent.
#[test]
fn digest_states() {
    let s = Scratch::new();
    let root = s.root();
    let no = AtomicBool::new(false);
    s.file(".envrc", b"export A=1\n");
    let expected: [u8; 32] = {
        use sha2::Digest;
        sha2::Sha256::digest(b"export A=1\n").into()
    };
    assert_eq!(walk::digest(&root, ".envrc", &no), FileState::Digest(expected));
    symlink("/etc/hosts", root.join(".gitmodules")).unwrap();
    let FileState::Digest(link) = walk::digest(&root, ".gitmodules", &no) else {
        panic!("link digest");
    };
    assert_ne!(link, expected);
    let fifo = std::ffi::CString::new(root.join(".gitattributes").to_str().unwrap()).unwrap();
    // SAFETY: a valid NUL-terminated path; mkfifo only creates the node.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert_eq!(
        walk::digest(&root, ".gitattributes", &no),
        FileState::Unreadable
    );
    s.file(".git/hooks/big", &vec![b'x'; lists::MAX_HASHED_BYTES as usize + 1]);
    assert_eq!(walk::digest(&root, ".git/hooks/big", &no), FileState::TooLarge);
    s.file(".git/hooks/edge", &vec![b'x'; lists::MAX_HASHED_BYTES as usize]);
    assert!(matches!(
        walk::digest(&root, ".git/hooks/edge", &no),
        FileState::Digest(_)
    ));
    assert_eq!(walk::digest(&root, ".git/hooks/none", &no), FileState::Missing);
    std::fs::create_dir(s.0.join("outside")).unwrap();
    std::fs::write(s.0.join("outside/.envrc"), b"x").unwrap();
    symlink(s.0.join("outside"), root.join("linked")).unwrap();
    assert_eq!(
        walk::digest(&root, "linked/.envrc", &no),
        FileState::Unreadable
    );
}

fn root_of(s: &Scratch) -> Root {
    let runtime = asura_platform::RuntimeDirectory::scratch(&s.0, true).unwrap();
    let path = s.root().to_string_lossy().into_owned();
    let boundary = runtime.state_boundary();
    let identity = asura_platform::ProjectIdentity::open(&path, &boundary).unwrap();
    Root {
        path,
        device: identity.device,
        inode: identity.inode,
        boundary,
    }
}
fn wait(ticket: &worker::Ticket) -> Reply {
    let start = Instant::now();
    loop {
        match ticket.poll() {
            Polled::Ready(reply) => return reply,
            Polled::Lost => panic!("worker lost"),
            Polled::Pending => {
                assert!(start.elapsed() < Duration::from_secs(30), "no reply");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

/// The worker: a check reads the implicit files and approved paths; a
/// replaced project is unavailable; the queue admits eight; a stalled
/// request keeps the queue busy until cancelled.
#[test]
fn worker_check_admission_and_cancel() {
    let s = Scratch::new();
    s.file(".envrc", b"a");
    s.file(".git/hooks/pre-commit", b"b");
    let root = root_of(&s);
    let gate = s.0.join("gate");
    let mut guard = Guard::start(None, Some(gate.clone())).unwrap();
    let ticket = guard
        .submit(
            root.clone(),
            Work::Check {
                approved: vec![".envrc".into(), ".git/hooks/gone".into()],
            },
        )
        .unwrap();
    let Reply::Checked(facts) = wait(&ticket) else {
        panic!("checked");
    };
    assert_eq!(
        facts.files.keys().map(String::as_str).collect::<Vec<_>>(),
        [".envrc", ".git/hooks/pre-commit"]
    );
    assert_eq!(
        facts.approved,
        BTreeMap::from([(".git/hooks/gone".to_owned(), FileState::Missing)])
    );
    let ticket = guard
        .submit(
            root.clone(),
            Work::Digest {
                path: ".envrc".into(),
            },
        )
        .unwrap();
    assert!(matches!(
        wait(&ticket),
        Reply::Digest(FileState::Digest(_), Some(shown)) if shown == ".envrc"
    ));
    // A replaced project directory is not read.
    let moved = Root {
        inode: root.inode + 1,
        ..root.clone()
    };
    let ticket = guard.submit(moved, Work::Check { approved: vec![] }).unwrap();
    assert_eq!(wait(&ticket), Reply::Unavailable);
    // A stalled filesystem (the gate): eight admitted, the ninth busy.
    std::fs::write(&gate, b"").unwrap();
    let mut held = Vec::new();
    for _ in 0..worker::QUEUE_BOUND {
        held.push(
            guard
                .submit(root.clone(), Work::Check { approved: vec![] })
                .unwrap(),
        );
    }
    assert!(!guard.has_room());
    assert_eq!(
        guard
            .submit(root.clone(), Work::Check { approved: vec![] })
            .err(),
        Some(Refused::Busy)
    );
    // Dropping the tickets cancels the requests; the queue drains.
    drop(held);
    let start = Instant::now();
    while !guard.has_room() {
        assert!(start.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(2));
    }
    std::fs::remove_file(&gate).unwrap();
    let ticket = guard
        .submit(root.clone(), Work::Check { approved: vec![] })
        .unwrap();
    assert!(matches!(wait(&ticket), Reply::Checked(_)));
    guard.close();
    assert_eq!(
        guard.submit(root, Work::Check { approved: vec![] }).err(),
        Some(Refused::Stopped)
    );
}
