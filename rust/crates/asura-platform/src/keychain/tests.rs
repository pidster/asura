use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;

const INSTALLATION: [u8; 16] = [0xa5; 16];

fn key(input: u8) -> ItemKey {
    ItemKey {
        installation: INSTALLATION,
        input: [input; 16],
    }
}

/// A recording fixture of the Security framework calls: each call is logged
/// and answered from a script.
#[derive(Default)]
struct Fixture {
    log: RefCell<Vec<&'static str>>,
    statuses: RefCell<VecDeque<Result<u32, i32>>>,
    item: RefCell<Option<Result<(), i32>>>,
    data: RefCell<Option<Result<Vec<u8>, i32>>>,
    accounts: RefCell<Option<Result<Vec<String>, i32>>>,
    /// The `max` of every list call.
    list_max: RefCell<Vec<usize>>,
}

impl Fixture {
    fn statuses(statuses: &[Result<u32, i32>]) -> Self {
        let fixture = Self::default();
        fixture
            .statuses
            .borrow_mut()
            .extend(statuses.iter().copied());
        fixture
    }
    fn item(self, result: Result<(), i32>) -> Self {
        *self.item.borrow_mut() = Some(result);
        self
    }
    fn data(self, result: Result<Vec<u8>, i32>) -> Self {
        *self.data.borrow_mut() = Some(result);
        self
    }
    fn accounts(self, result: Result<Vec<String>, i32>) -> Self {
        *self.accounts.borrow_mut() = Some(result);
        self
    }
    fn log(&self) -> Vec<&'static str> {
        self.log.borrow().clone()
    }
}

impl Calls for Fixture {
    fn status(&self) -> Result<u32, i32> {
        self.log.borrow_mut().push("status");
        self.statuses
            .borrow_mut()
            .pop_front()
            .expect("unscripted status call")
    }
    fn add(&self, service: &str, account: &str, _: &[u8]) -> Result<(), i32> {
        assert!(service.starts_with(SERVICE_PREFIX) && account.starts_with(ACCOUNT_PREFIX));
        self.log.borrow_mut().push("add");
        self.item.borrow_mut().take().expect("unscripted add")
    }
    fn copy(&self, _: &str, _: &str) -> Result<Zeroizing<Vec<u8>>, i32> {
        self.log.borrow_mut().push("copy");
        self.data
            .borrow_mut()
            .take()
            .expect("unscripted copy")
            .map(Zeroizing::new)
    }
    fn delete(&self, _: &str, _: &str) -> Result<(), i32> {
        self.log.borrow_mut().push("delete");
        self.item.borrow_mut().take().expect("unscripted delete")
    }
    fn accounts(&self, service: &str, max: usize) -> Result<Vec<String>, i32> {
        assert_eq!(service, service_name(&INSTALLATION));
        self.log.borrow_mut().push("list");
        self.list_max.borrow_mut().push(max);
        self.accounts.borrow_mut().take().expect("unscripted list")
    }
}

#[test]
fn item_names_follow_the_contract() {
    let key = ItemKey {
        installation: [0x01; 16],
        input: [0xab; 16],
    };
    assert_eq!(
        key.service(),
        "asura.secret-store.01010101010101010101010101010101"
    );
    assert_eq!(key.account(), "input.abababababababababababababababab");
    assert_eq!(ItemKey::parse(&key.service(), &key.account()), Some(key));
    for (service, account) in [
        (key.service().to_uppercase(), key.account()),
        (key.service(), key.account().to_uppercase()),
        (key.service(), "input.abab".to_owned()),
        (key.service(), format!("{}00", key.account())),
        (key.service(), key.account().replace("input.", "other.")),
        (
            "asura.other.01010101010101010101010101010101".to_owned(),
            key.account(),
        ),
    ] {
        assert_eq!(
            ItemKey::parse(&service, &account),
            None,
            "{service} {account}"
        );
    }
}

/// SS-U9: a non-zero pre-check prevents every item call.
#[test]
fn nonzero_precheck_prevents_every_item_call() {
    let missing = Err(-25_294);
    let expect = Err(Failure::Unavailable {
        status: -25_294,
        item_call: false,
    });
    let f = Fixture::statuses(&[missing]);
    assert_eq!(add_with(&f, &key(1), b"v"), expect);
    assert_eq!(f.log(), ["status"]);
    let f = Fixture::statuses(&[missing]);
    assert_eq!(get_with(&f, &key(1)), expect.map(|_| Found::Missing));
    assert_eq!(f.log(), ["status"]);
    let f = Fixture::statuses(&[missing]);
    assert_eq!(delete_with(&f, &key(1)), expect.map(|_| Removed::Absent));
    assert_eq!(f.log(), ["status"]);
    let f = Fixture::statuses(&[Err(100_000)]);
    assert_eq!(
        list_with(&f, &INSTALLATION),
        Err(Failure::Unavailable {
            status: 100_000,
            item_call: false
        })
    );
    assert_eq!(f.log(), ["status"]);
}

/// SS-U9: every item call follows a pre-check of 0 in the same call.
#[test]
fn every_item_call_follows_a_precheck() {
    let f = Fixture::statuses(&[Ok(0x7)]).item(Ok(()));
    assert_eq!(add_with(&f, &key(1), b"v"), Ok(Added::Stored));
    assert_eq!(f.log(), ["status", "add"]);
    let f = Fixture::statuses(&[Ok(0x7)]).data(Ok(b"v".to_vec()));
    assert_eq!(
        get_with(&f, &key(1)),
        Ok(Found::Data(Zeroizing::new(b"v".to_vec())))
    );
    assert_eq!(f.log(), ["status", "copy"]);
    let f = Fixture::statuses(&[Ok(0x2)]).item(Ok(()));
    assert_eq!(delete_with(&f, &key(1)), Ok(Removed::Deleted));
    assert_eq!(f.log(), ["status", "delete"]);
    let f = Fixture::statuses(&[Ok(0x2)]).accounts(Ok(vec![key(3).account()]));
    assert_eq!(
        list_with(&f, &INSTALLATION),
        Ok(Listing {
            inputs: vec![[3; 16]],
            unrecognized: 0,
            truncated: false,
        })
    );
    assert_eq!(f.log(), ["status", "list"]);
}

/// SS-K2-1: the search asks for at most 257 results; 257 results set
/// `truncated`; fewer leave it false.
#[test]
fn list_is_bounded_and_reports_truncation() {
    assert_eq!(LIST_BOUND, 257);
    let accounts = |n: usize, unrecognized: usize| -> Vec<String> {
        let mut accounts: Vec<String> = (0..n - unrecognized)
            .map(|i| {
                let mut input = [0u8; 16];
                input[..8].copy_from_slice(&(i as u64).to_be_bytes());
                ItemKey {
                    installation: INSTALLATION,
                    input,
                }
                .account()
            })
            .collect();
        accounts.extend((0..unrecognized).map(|i| format!("other.{i}")));
        accounts
    };
    for (n, unrecognized, truncated) in [
        (1, 0, false),
        (256, 0, false),
        (256, 2, false),
        (257, 0, true),
        (257, 3, true),
    ] {
        let f = Fixture::statuses(&[Ok(0x7)]).accounts(Ok(accounts(n, unrecognized)));
        let listing = list_with(&f, &INSTALLATION).expect("listing");
        assert_eq!(*f.list_max.borrow(), [257], "{n}");
        assert_eq!(listing.inputs.len(), n - unrecognized, "{n}");
        assert_eq!(listing.unrecognized, unrecognized, "{n}");
        assert_eq!(listing.truncated, truncated, "{n}");
    }
    // No match is an empty listing, not truncated.
    let f = Fixture::statuses(&[Ok(0x7)]).accounts(Err(-25_300));
    assert_eq!(list_with(&f, &INSTALLATION), Ok(Listing::default()));
    assert_eq!(*f.list_max.borrow(), [257]);
    // More results than asked for is a shape the request cannot produce.
    let f = Fixture::statuses(&[Ok(0x7)]).accounts(Ok(accounts(258, 0)));
    assert_eq!(
        list_with(&f, &INSTALLATION),
        Err(Failure::Unavailable {
            status: UNEXPECTED_RESULT_STATUS,
            item_call: true
        })
    );
}

/// SS-U3: -25293 and -25308 map by the unlock bit of a second status call.
#[test]
fn auth_failures_map_by_unlock_bit() {
    for code in [-25_293, -25_308] {
        let f = Fixture::statuses(&[Ok(0x2), Ok(0x2)]).data(Err(code));
        assert_eq!(get_with(&f, &key(1)), Err(Failure::Locked));
        assert_eq!(f.log(), ["status", "copy", "status"]);
        let f = Fixture::statuses(&[Ok(0x7), Ok(0x7)]).data(Err(code));
        assert_eq!(get_with(&f, &key(1)), Err(Failure::Denied));
        let f = Fixture::statuses(&[Ok(0x2), Ok(0x2)]).item(Err(code));
        assert_eq!(add_with(&f, &key(1), b"v"), Err(Failure::Locked));
        let f = Fixture::statuses(&[Ok(0x7), Ok(0x7)]).item(Err(code));
        assert_eq!(delete_with(&f, &key(1)), Err(Failure::Denied));
        let f = Fixture::statuses(&[Ok(0x7), Err(-25_294)]).item(Err(code));
        assert_eq!(
            add_with(&f, &key(1), b"v"),
            Err(Failure::Unavailable {
                status: -25_294,
                item_call: true
            })
        );
    }
}

/// SS-U3: the remaining rows of the revised status table.
#[test]
fn status_table_rows() {
    let f = Fixture::statuses(&[Ok(0x7)]).item(Err(-25_299));
    assert_eq!(add_with(&f, &key(1), b"v"), Ok(Added::Duplicate));
    let f = Fixture::statuses(&[Ok(0x7)]).data(Err(-25_300));
    assert_eq!(get_with(&f, &key(1)), Ok(Found::Missing));
    let f = Fixture::statuses(&[Ok(0x7)]).item(Err(-25_300));
    assert_eq!(delete_with(&f, &key(1)), Ok(Removed::Absent));
    let f = Fixture::statuses(&[Ok(0x7)]).accounts(Err(-25_300));
    assert_eq!(list_with(&f, &INSTALLATION), Ok(Listing::default()));
    let f = Fixture::statuses(&[Ok(0x7)]).item(Err(-25_244));
    assert_eq!(delete_with(&f, &key(1)), Err(Failure::Denied));
    assert_eq!(f.log(), ["status", "delete"]);
    for code in [-25_294, -25_291, UNEXPECTED_RESULT_STATUS] {
        let f = Fixture::statuses(&[Ok(0x7)]).data(Err(code));
        assert_eq!(
            get_with(&f, &key(1)),
            Err(Failure::Unavailable {
                status: code,
                item_call: true
            })
        );
        assert_eq!(f.log(), ["status", "copy"]);
    }
    let f = Fixture::statuses(&[Ok(0x7)]).accounts(Ok(vec![
        key(4).account(),
        "input.zz".to_owned(),
        key(5).account(),
    ]));
    assert_eq!(
        list_with(&f, &INSTALLATION),
        Ok(Listing {
            inputs: vec![[4; 16], [5; 16]],
            unrecognized: 1,
            truncated: false,
        })
    );
}

#[test]
fn add_bounds_need_no_keychain_call() {
    for size in [0, MAX_ITEM_BYTES + 1] {
        let f = Fixture::default();
        assert_eq!(add_with(&f, &key(1), &vec![1; size]), Err(Failure::Invalid));
        assert!(f.log().is_empty());
    }
    let f = Fixture::statuses(&[Ok(0x7)]).item(Ok(()));
    assert_eq!(
        add_with(&f, &key(1), &vec![1; MAX_ITEM_BYTES]),
        Ok(Added::Stored)
    );
}

#[test]
fn unit_tests_never_open_the_default_keychain() {
    assert_eq!(
        Keychain::open(&Target::Default).map(drop),
        Err(Failure::Invalid)
    );
}

/// Real Keychain cases over scratch keychain files (SSR6, R1-R5). Each test
/// owns one scratch directory and one keychain; nothing touches the login
/// keychain, the default keychain or the search list.
#[cfg(feature = "test-support")]
mod scratch_keychain {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;
    use std::process::Command;

    struct Scratch {
        dir: PathBuf,
        target: Option<Target>,
        search_list: String,
    }

    fn search_list() -> String {
        // Read-only: prints the person's keychain search list.
        let output = Command::new("/usr/bin/security")
            .args(["list-keychains", "-d", "user"])
            .output()
            .expect("security list-keychains");
        assert!(output.status.success());
        String::from_utf8(output.stdout).expect("utf-8 search list")
    }

    fn hex_id() -> String {
        crate::random_id()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    impl Scratch {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .canonicalize()
                .expect("temporary directory");
            let dir = root.join(format!("asura-kc-{}-{}", std::process::id(), hex_id()));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&dir)
                .expect("scratch directory");
            let search_list = search_list();
            let target =
                scratch::create(&dir.join("test.keychain"), &hex_id()).expect("scratch keychain");
            Self {
                dir,
                target: Some(target),
                search_list,
            }
        }
        fn target(&self) -> &Target {
            self.target.as_ref().expect("scratch keychain present")
        }
        fn open(&self) -> Keychain {
            let keychain = Keychain::open(self.target()).expect("open scratch keychain");
            assert_eq!(keychain.status().map(|_| ()), Ok(()));
            keychain
        }
        fn path(&self) -> &Path {
            match self.target() {
                Target::Scratch { path, .. } => path,
                Target::Default => unreachable!(),
            }
        }
        /// Teardown after every keychain user has stopped (testing rule 2).
        fn delete_keychain(&mut self) {
            if let Some(target) = self.target.take() {
                assert_eq!(scratch::delete(&target), Ok(()));
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Some(target) = self.target.take() {
                let _ = scratch::delete(&target);
            }
            let _ = std::fs::remove_dir_all(&self.dir);
            if !std::thread::panicking() {
                assert_eq!(search_list(), self.search_list, "search list changed");
            }
        }
    }

    fn payload(size: usize) -> Vec<u8> {
        (0..size).map(|i| (i * 31 + 7) as u8).collect()
    }

    /// SS-P1 and R5: round trip of the largest payload; list with no items.
    #[test]
    fn round_trip_largest_payload_and_empty_list() {
        let mut scratch = Scratch::new();
        let keychain = scratch.open();
        assert_eq!(keychain.list(&INSTALLATION), Ok(Listing::default()));
        let data = payload(MAX_ITEM_BYTES);
        assert_eq!(keychain.add(&key(1), &data), Ok(Added::Stored));
        assert_eq!(keychain.add(&key(1), &data), Ok(Added::Duplicate));
        assert_eq!(keychain.get(&key(1)), Ok(Found::Data(Zeroizing::new(data))));
        assert_eq!(keychain.get(&key(2)), Ok(Found::Missing));
        assert_eq!(
            keychain.list(&INSTALLATION),
            Ok(Listing {
                inputs: vec![[1; 16]],
                unrecognized: 0,
                truncated: false,
            })
        );
        assert_eq!(keychain.list(&[0x5a; 16]), Ok(Listing::default()));
        assert_eq!(keychain.delete(&key(1)), Ok(Removed::Deleted));
        assert_eq!(keychain.delete(&key(1)), Ok(Removed::Absent));
        assert_eq!(keychain.get(&key(1)), Ok(Found::Missing));
        assert_eq!(keychain.list(&INSTALLATION), Ok(Listing::default()));
        drop(keychain);
        scratch.delete_keychain();
    }

    /// R1 and R4: locked keychain refuses get and add as `Locked` after a
    /// pre-check of 0; list and delete succeed while locked.
    #[test]
    fn locked_keychain() {
        let mut scratch = Scratch::new();
        let keychain = scratch.open();
        assert_eq!(keychain.add(&key(1), b"one"), Ok(Added::Stored));
        assert_eq!(keychain.add(&key(2), b"two"), Ok(Added::Stored));
        assert_eq!(scratch::lock(scratch.target()), Ok(()));
        let flags = keychain.status().expect("pre-check 0 while locked");
        assert_eq!(flags & STATUS_UNLOCKED, 0, "flags {flags:#x}");
        assert_eq!(keychain.get(&key(1)), Err(Failure::Locked));
        assert_eq!(keychain.add(&key(3), b"three"), Err(Failure::Locked));
        let listing = keychain.list(&INSTALLATION).expect("list while locked");
        let mut inputs = listing.inputs;
        inputs.sort();
        assert_eq!(inputs, [[1; 16], [2; 16]]);
        assert_eq!(keychain.delete(&key(1)), Ok(Removed::Deleted));
        assert_eq!(keychain.delete(&key(1)), Ok(Removed::Absent));
        assert_eq!(scratch::unlock(scratch.target()), Ok(()));
        let flags = keychain.status().expect("pre-check after unlock");
        assert_ne!(flags & STATUS_UNLOCKED, 0, "flags {flags:#x}");
        assert_eq!(
            keychain.get(&key(2)),
            Ok(Found::Data(Zeroizing::new(b"two".to_vec())))
        );
        assert_eq!(keychain.add(&key(3), b"three"), Ok(Added::Stored));
        drop(keychain);
        scratch.delete_keychain();
    }

    fn fixture_binary() -> PathBuf {
        let exe = std::env::current_exe().expect("test executable");
        let profile = exe
            .parent()
            .and_then(Path::parent)
            .expect("target profile directory");
        // A test-support `[[bin]]`: `cargo test` with the feature builds it
        // beside the test executables' `deps` directory.
        let fixture = profile.join("keychain_fixture");
        assert!(
            fixture.is_file(),
            "{} missing: run cargo test with --features test-support",
            fixture.display()
        );
        fixture
    }

    /// R2: an item written by the fixture binary is `Denied` to this binary.
    /// Delete is recorded: on macOS 27.0 another same-UID binary deletes it.
    #[test]
    fn item_of_another_binary_is_denied() {
        let mut scratch = Scratch::new();
        let Target::Scratch { password, .. } = scratch.target() else {
            unreachable!()
        };
        let output = Command::new(fixture_binary())
            .arg(scratch.path())
            .arg(password.as_str())
            .arg(key(7).service())
            .arg(key(7).account())
            .output()
            .expect("run keychain fixture");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "stored",
            "fixture stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success());
        let keychain = scratch.open();
        assert_eq!(keychain.get(&key(7)), Err(Failure::Denied));
        let deleted = keychain.delete(&key(7));
        assert!(
            matches!(deleted, Ok(Removed::Deleted) | Err(Failure::Denied)),
            "{deleted:?}"
        );
        eprintln!("R2 delete of another binary's item: {deleted:?}");
        drop(keychain);
        scratch.delete_keychain();
    }

    /// SS1-K2: `fixture_main` writes an account outside the item contract
    /// through `add_account`, and rejects a service outside it with exit 2
    /// and no write.
    #[test]
    fn fixture_writes_an_unrecognized_account() {
        let mut scratch = Scratch::new();
        let Target::Scratch { password, .. } = scratch.target() else {
            unreachable!()
        };
        let run = |service: &str, account: &str| {
            Command::new(fixture_binary())
                .arg(scratch.path())
                .arg(password.as_str())
                .arg(service)
                .arg(account)
                .output()
                .expect("run keychain fixture")
        };
        let output = run(&key(7).service(), "other");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "stored",
            "fixture stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success());
        let output = run(&key(7).service(), "other");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "duplicate");
        assert_eq!(output.status.code(), Some(1));
        let output = run("asura.other.service", "other");
        assert!(output.stdout.is_empty());
        assert_eq!(output.status.code(), Some(2));
        let keychain = scratch.open();
        assert_eq!(
            keychain.list(&INSTALLATION),
            Ok(Listing {
                inputs: vec![],
                unrecognized: 1,
                truncated: false,
            })
        );
        drop(keychain);
        scratch.delete_keychain();
    }

    /// SS1-K2: `add_account` writes only to a scratch target, with the same
    /// add replies as `Keychain::add`; list counts the account unrecognized.
    #[test]
    fn add_account_writes_an_unrecognized_account() {
        let mut scratch = Scratch::new();
        let keychain = scratch.open();
        let target = scratch.target();
        assert_eq!(
            scratch::add_account(&Target::Default, &INSTALLATION, "other", b"x"),
            Err(Failure::Invalid)
        );
        assert_eq!(
            scratch::add_account(target, &INSTALLATION, "", b"x"),
            Err(Failure::Invalid)
        );
        assert_eq!(
            scratch::add_account(target, &INSTALLATION, "other", b""),
            Err(Failure::Invalid)
        );
        assert_eq!(
            scratch::add_account(target, &INSTALLATION, "other", b"x"),
            Ok(Added::Stored)
        );
        assert_eq!(
            scratch::add_account(target, &INSTALLATION, "other", b"x"),
            Ok(Added::Duplicate)
        );
        assert_eq!(keychain.add(&key(1), b"one"), Ok(Added::Stored));
        assert_eq!(
            keychain.list(&INSTALLATION),
            Ok(Listing {
                inputs: vec![[1; 16]],
                unrecognized: 1,
                truncated: false,
            })
        );
        assert_eq!(keychain.list(&[0x5a; 16]), Ok(Listing::default()));
        drop(keychain);
        scratch.delete_keychain();
    }

    /// SS1-K2 and SS-P3: a numeric match limit caps a real file-keychain
    /// list; a larger limit returns every item.
    #[test]
    fn match_limit_caps_the_list() {
        let mut scratch = Scratch::new();
        let keychain = scratch.open();
        for input in 1..=3 {
            assert_eq!(keychain.add(&key(input), b"v"), Ok(Added::Stored));
        }
        let service = service_name(&INSTALLATION);
        assert_eq!(keychain.status().map(drop), Ok(()));
        assert_eq!(
            Calls::accounts(&keychain, &service, 2).map(|a| a.len()),
            Ok(2)
        );
        assert_eq!(
            Calls::accounts(&keychain, &service, LIST_BOUND).map(|a| a.len()),
            Ok(3)
        );
        let listing = keychain.list(&INSTALLATION).expect("listing");
        assert_eq!((listing.inputs.len(), listing.truncated), (3, false));
        drop(keychain);
        scratch.delete_keychain();
    }

    /// R3: after the scratch keychain is deleted, only the pre-check runs and
    /// every request is `Unavailable` with no item call. No add is attempted.
    #[test]
    fn deleted_keychain_fails_the_precheck() {
        let mut scratch = Scratch::new();
        let target = scratch.target().clone();
        let keychain = scratch.open();
        assert_eq!(keychain.add(&key(1), b"one"), Ok(Added::Stored));
        drop(keychain);
        scratch.delete_keychain();
        let keychain = Keychain::open(&target).expect("open succeeds on a missing path");
        let Err(Failure::Unavailable {
            status,
            item_call: false,
        }) = keychain.status()
        else {
            panic!("pre-check of a deleted keychain returned 0");
        };
        assert_ne!(status, 0);
        let unavailable = Failure::Unavailable {
            status,
            item_call: false,
        };
        assert_eq!(keychain.get(&key(1)), Err(unavailable));
        assert_eq!(keychain.delete(&key(1)), Err(unavailable));
        assert_eq!(keychain.list(&INSTALLATION), Err(unavailable));
        assert_eq!(scratch::lock(&target), Err(unavailable));
        assert_eq!(scratch::delete(&target), Err(unavailable));
    }

    #[test]
    fn scratch_paths_are_restricted() {
        let scratch = Scratch::new();
        let home = std::env::var_os("HOME").expect("HOME");
        for path in [
            PathBuf::from("relative.keychain"),
            scratch.dir.join("wrong-suffix.keychain-db"),
            Path::new(&home).join("Library/Keychains/asura-test.keychain"),
            PathBuf::from("/Library/Keychains/asura-test.keychain"),
            scratch.path().to_path_buf(),
        ] {
            assert_eq!(
                scratch::create(&path, "pw").map(drop),
                Err(Failure::Invalid),
                "{}",
                path.display()
            );
        }
        let wrong = Target::Scratch {
            path: Path::new(&home).join("Library/Keychains/login.keychain-db"),
            password: Zeroizing::new("pw".to_owned()),
        };
        assert_eq!(Keychain::open(&wrong).map(drop), Err(Failure::Invalid));
        assert_eq!(scratch::lock(&Target::Default), Err(Failure::Invalid));
        assert_eq!(scratch::delete(&Target::Default), Err(Failure::Invalid));
        assert!(!format!("{:?}", scratch.target()).contains("pw"));
    }

    #[test]
    fn user_interaction_stays_disabled() {
        let scratch = Scratch::new();
        let _keychain = scratch.open();
        assert_eq!(
            SecKeychain::user_interaction_allowed().map_err(|e| e.code()),
            Ok(false)
        );
    }
}
