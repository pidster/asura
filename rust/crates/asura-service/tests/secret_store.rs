//! SS1-S worker-layer integration (`docs/designs/secret-store.md`, SS-W1 to
//! SS-W10): the real secret store worker over the real
//! `asura_platform::keychain` and a scratch keychain file.
//!
//! Testing rules (SSR6): every case uses its own scratch keychain under the
//! temporary directory through `Target::Scratch`; nothing reads, writes or
//! deletes a login keychain item; every add is pre-checked by the platform
//! in the same call; user interaction is disabled; teardown stops the worker
//! before it deletes the scratch keychain and checks that the person's
//! search list is unchanged.
use asura_platform::PollInterest;
use asura_platform::events::wake_pair;
use asura_platform::keychain::{
    Added, ItemKey, Keychain, LIST_BOUND, MAX_ITEM_BYTES, Target, scratch, service_name,
};
use asura_service::secret_store::{
    Call, DeleteReply, Gate, GetReply, Polled, PutReply, QUEUE_BOUND, Refused, SecretValue,
    Secrets, Store, StoreFailure, SweepReport,
};
use std::collections::BTreeSet;
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const BINDING: [u8; 32] = [0x5b; 32];

fn random_hex() -> String {
    asura_platform::random_id()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn id(n: u16) -> [u8; 16] {
    let mut id = [0u8; 16];
    id[0] = 0xa5;
    id[14..].copy_from_slice(&n.to_be_bytes());
    id
}

fn hex(id: &[u8; 16]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

/// Read-only: the person's keychain search list.
fn search_list() -> String {
    let output = Command::new("/usr/bin/security")
        .args(["list-keychains", "-d", "user"])
        .output()
        .expect("security list-keychains");
    assert!(output.status.success());
    String::from_utf8(output.stdout).expect("utf-8 search list")
}

/// One scratch keychain file in its own private directory.
struct Scratch {
    dir: PathBuf,
    target: Option<Target>,
    password: String,
    installation: [u8; 16],
    search_list: String,
}

impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir()
            .canonicalize()
            .expect("temporary directory");
        let dir = root.join(format!("asura-ss-{}-{}", std::process::id(), random_hex()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .expect("scratch directory");
        let search_list = search_list();
        let password = random_hex();
        let target =
            scratch::create(&dir.join("store.keychain"), &password).expect("scratch keychain");
        Self {
            dir,
            target: Some(target),
            password,
            installation: asura_platform::random_id(),
            search_list,
        }
    }
    fn target(&self) -> Target {
        self.target.clone().expect("scratch keychain present")
    }
    fn path(&self) -> PathBuf {
        match self.target() {
            Target::Scratch { path, .. } => path,
            Target::Default => unreachable!("tests never use the default target"),
        }
    }
    fn store(&self) -> Store {
        Store::new(self.target(), self.installation, None).expect("secret store worker")
    }
    fn gated(&self, gate: &Gate) -> Store {
        Store::gated(self.target(), self.installation, None, gate.clone())
            .expect("secret store worker")
    }
    /// The test's own handle on the scratch keychain, for fixtures and
    /// independent checks. Every call it makes pre-checks the target.
    fn keychain(&self) -> Keychain {
        let keychain = Keychain::open(&self.target()).expect("open scratch keychain");
        assert_eq!(keychain.status().map(drop), Ok(()));
        keychain
    }
    fn fill(&self, inputs: impl IntoIterator<Item = u16>) {
        let keychain = self.keychain();
        for n in inputs {
            let key = ItemKey {
                installation: self.installation,
                input: id(n),
            };
            assert_eq!(keychain.add(&key, b"fill"), Ok(Added::Stored));
        }
    }
    fn listed(&self) -> usize {
        let listing = self.keychain().list(&self.installation).expect("list");
        listing.inputs.len() + listing.unrecognized
    }
    /// Writes one item with the fixture binary, a different executable.
    fn fixture(&self, account: &str) {
        let output = Command::new(env!("CARGO_BIN_EXE_secret_store_fixture"))
            .arg(self.path())
            .arg(&self.password)
            .arg(service_name(&self.installation))
            .arg(account)
            .output()
            .expect("run secret store fixture");
        assert!(
            output.status.success(),
            "fixture stdout: {} stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "stored");
    }
    /// Teardown after every worker on the target has stopped (rule 2).
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

/// Stops the worker and waits for its thread (testing rule 2).
fn stop(store: Store) {
    store.close().join().expect("worker exits");
}

fn bytes(len: usize, seed: u8) -> Zeroizing<Vec<u8>> {
    Zeroizing::new(
        (0..len)
            .map(|i| (i as u8).wrapping_mul(29) ^ seed)
            .collect(),
    )
}

/// Values whose payload is exactly 8,307 bytes.
fn largest() -> Secrets {
    Secrets {
        variables: (1..=16)
            .map(|number| SecretValue {
                number,
                value: bytes(512, number),
            })
            .collect(),
        whole_command: None,
    }
}

fn small() -> Secrets {
    Secrets {
        variables: vec![SecretValue {
            number: 1,
            value: bytes(16, 7),
        }],
        ..Secrets::default()
    }
}

fn put(store: &Store, input: u16, secrets: Secrets) -> PutReply {
    store
        .put(id(input), BINDING, secrets)
        .expect("admitted")
        .wait()
        .expect("reply")
}
fn get(store: &Store, input: u16) -> GetReply {
    store
        .get(id(input), BINDING)
        .expect("admitted")
        .wait()
        .expect("reply")
}
fn delete(store: &Store, input: u16) -> DeleteReply {
    store
        .delete(id(input))
        .expect("admitted")
        .wait()
        .expect("reply")
}
fn sweep(store: &Store, live: impl IntoIterator<Item = u16>) -> SweepReport {
    store
        .sweep(live.into_iter().map(id).collect::<BTreeSet<_>>())
        .expect("admitted")
        .wait()
        .expect("reply")
}

/// SS-W1: put, get, delete and list a 55,413-byte payload.
#[test]
fn ss_w1_largest_payload_round_trip() {
    let mut scratch = Scratch::new();
    let store = scratch.store();
    let values = largest();
    assert_eq!(put(&store, 1, values.clone()), PutReply::Stored);
    assert_eq!(scratch.listed(), 1);
    assert_eq!(get(&store, 1), GetReply::Values(values));
    assert_eq!(delete(&store, 1), DeleteReply::Deleted);
    assert_eq!(delete(&store, 1), DeleteReply::Absent);
    let report = sweep(&store, []);
    assert_eq!(
        (report.listings, report.deleted, report.outcome),
        (1, 0, Ok(()))
    );
    assert_eq!(store.observation().item_count, Some(0));
    assert_eq!(scratch.listed(), 0, "zero items listed afterwards");
    stop(store);
    scratch.delete_keychain();
}

/// SS-W1 payload size check: the values above encode to the item bound.
#[test]
fn largest_values_fill_the_item_bound() {
    // 1 version + 32 binding + 1 flags + 8,241 variables + 32 key (ENV1
    // removed the launch environment).
    let variables = 1 + 16 * 3 + 16 * 512;
    assert_eq!(1 + 32 + 1 + variables + 32, MAX_ITEM_BYTES);
}

/// SS-W2: a locked keychain gives `locked` for get and put; list and delete
/// succeed while locked.
#[test]
fn ss_w2_locked_keychain() {
    let mut scratch = Scratch::new();
    let store = scratch.store();
    assert_eq!(put(&store, 1, small()), PutReply::Stored);
    stop(store);
    scratch::lock(&scratch.target()).expect("lock scratch keychain");
    let store = scratch.store();
    // SG3: the locked reply carries the raw status and the session state.
    let locked = get(&store, 1);
    assert!(
        matches!(
            locked,
            GetReply::Failed(StoreFailure::Locked {
                status: -25_293 | -25_308,
                session: Some(_),
            })
        ),
        "{locked:?}"
    );
    assert!(matches!(
        put(&store, 2, small()),
        PutReply::Failed(StoreFailure::Locked { .. })
    ));
    let report = sweep(&store, []);
    assert_eq!((report.deleted, report.outcome), (1, Ok(())));
    assert_eq!(delete(&store, 1), DeleteReply::Absent);
    stop(store);
    scratch::unlock(&scratch.target()).expect("unlock scratch keychain");
    assert_eq!(scratch.listed(), 0);
    scratch.delete_keychain();
}

/// SS-W3: an item the fixture binary wrote is `denied` to this binary,
/// with no dialog (user interaction is disabled).
#[test]
fn ss_w3_item_of_another_binary_is_denied() {
    let mut scratch = Scratch::new();
    scratch.fixture(&format!("input.{}", hex(&id(1))));
    let store = scratch.store();
    let denied = get(&store, 1);
    assert!(
        matches!(
            denied,
            GetReply::Failed(StoreFailure::Denied {
                session: Some(_),
                ..
            })
        ),
        "{denied:?}"
    );
    stop(store);
    scratch.delete_keychain();
}

/// SS-W4: with the worker blocked inside a Keychain call, the 17th request
/// gets `busy` and the caller's reactor fixture stays responsive; the
/// worker's replies wake the reactor after the release.
#[test]
fn ss_w4_blocked_worker_keeps_the_caller_responsive() {
    let mut scratch = Scratch::new();
    let gate = Gate::default();
    let (reader, wake) = wake_pair().expect("wake pair");
    let store = Store::gated(
        scratch.target(),
        scratch.installation,
        Some(wake),
        gate.clone(),
    )
    .expect("secret store worker");
    gate.hold();
    let mut tickets = vec![store.get(id(1), BINDING).expect("admitted")];
    gate.wait_blocked();
    // The reactor fixture: every call returns without waiting for the
    // worker. 1 s per call is an acceptance figure, not a runtime bound.
    let mut slowest = Duration::ZERO;
    let mut timed = |work: &mut dyn FnMut()| {
        let start = Instant::now();
        work();
        slowest = slowest.max(start.elapsed());
    };
    for n in 2..=QUEUE_BOUND as u16 {
        timed(&mut || tickets.push(store.get(id(n), BINDING).expect("admitted")));
    }
    timed(&mut || assert_eq!(store.get(id(17), BINDING).err(), Some(Refused::Busy)));
    timed(&mut || {
        assert_eq!(
            store.put(id(18), BINDING, small()).err(),
            Some(Refused::Busy)
        )
    });
    timed(&mut || assert_eq!(store.delete(id(19)).err(), Some(Refused::Busy)));
    timed(&mut || assert!(!store.has_room()));
    timed(&mut || assert_eq!(store.observation().item_count, None));
    timed(&mut || assert_eq!(tickets[0].poll(), Polled::Pending));
    assert!(slowest < Duration::from_secs(1), "slowest call {slowest:?}");

    gate.release();
    // The loop ends when every ticket has its reply; the 100 ms poll of the
    // wake descriptor is pacing and ends nothing.
    let mut replies = 0;
    while !tickets.is_empty() {
        asura_platform::poll(
            &[PollInterest {
                fd: reader.as_raw_fd(),
                read: true,
                write: false,
            }],
            Duration::from_millis(100),
        )
        .expect("poll wake");
        reader.drain().expect("drain wake");
        tickets.retain(|ticket| match ticket.poll() {
            Polled::Ready(reply) => {
                assert_eq!(reply, GetReply::Missing);
                replies += 1;
                false
            }
            Polled::Pending => true,
            Polled::Lost => panic!("worker exited without a reply"),
        });
    }
    assert_eq!(replies, QUEUE_BOUND);
    assert!(store.has_room());
    assert_eq!(gate.calls(), vec![Call::Get; QUEUE_BOUND]);
    stop(store);
    scratch.delete_keychain();
}

/// SS-W5: on a deleted scratch keychain the pre-check fails: get and sweep
/// are `unavailable`, and no add is made. Rule 3: this case never calls put.
#[test]
fn ss_w5_deleted_keychain_fails_the_precheck() {
    let mut scratch = Scratch::new();
    let store = scratch.store();
    assert_eq!(put(&store, 1, small()), PutReply::Stored);
    stop(store);
    let target = scratch.target();
    scratch.delete_keychain();
    let gate = Gate::default();
    let store = Store::gated(target, scratch.installation, None, gate.clone())
        .expect("secret store worker");
    let GetReply::Failed(StoreFailure::Unavailable { status, .. }) = get(&store, 1) else {
        panic!("get on a deleted keychain was not unavailable");
    };
    assert_ne!(status, 0);
    let report = sweep(&store, []);
    assert!(matches!(
        report.outcome,
        Err(StoreFailure::Unavailable { status, .. }) if status != 0
    ));
    let observation = store.observation();
    assert!(observation.sweep_pending);
    assert_eq!(observation.item_count, None);
    assert_ne!(observation.last_status, 0);
    stop(store);
    assert!(!gate.calls().contains(&Call::Add), "no add is made");
}

/// SS-W6: a sweep with a live set over 3 items keeps the live one, deletes
/// the others and updates the observation.
#[test]
fn ss_w6_sweep_keeps_the_live_item() {
    let mut scratch = Scratch::new();
    let store = scratch.store();
    for n in 1..=3 {
        assert_eq!(put(&store, n, small()), PutReply::Stored);
    }
    stop(store);
    // A restarted worker sweeps with the replay's live set.
    let store = scratch.store();
    let report = sweep(&store, [2]);
    assert_eq!(
        (report.kept, report.deleted, report.outcome),
        (1, 2, Ok(()))
    );
    let observation = store.observation();
    assert_eq!(observation.item_count, Some(1));
    assert_eq!(observation.unrecognized_count, 0);
    assert_eq!(get(&store, 2), GetReply::Values(small()));
    assert_eq!(get(&store, 1), GetReply::Missing);
    stop(store);
    scratch.delete_keychain();
}

/// SS-W7: with 256 items present, put is `full` and makes no add.
#[test]
fn ss_w7_full_at_256_items() {
    let mut scratch = Scratch::new();
    scratch.fill(1..=256);
    let gate = Gate::default();
    let store = scratch.gated(&gate);
    assert_eq!(put(&store, 300, small()), PutReply::Full);
    assert!(!gate.calls().contains(&Call::Add));
    stop(store);
    assert_eq!(scratch.listed(), 256);
    scratch.delete_keychain();
}

/// SS-W8: a delete that fails once keeps the item; the next sweep deletes it.
#[test]
fn ss_w8_failed_delete_is_swept() {
    let mut scratch = Scratch::new();
    let store = scratch.store();
    assert_eq!(put(&store, 1, small()), PutReply::Stored);
    stop(store);
    let gate = Gate::default();
    let store = scratch.gated(&gate);
    gate.fail_once(Call::Delete);
    assert!(matches!(
        delete(&store, 1),
        DeleteReply::Failed(StoreFailure::Unavailable { .. })
    ));
    assert_eq!(scratch.listed(), 1, "item kept");
    let report = sweep(&store, []);
    assert_eq!((report.deleted, report.outcome), (1, Ok(())));
    assert_eq!(scratch.listed(), 0);
    stop(store);
    scratch.delete_keychain();
}

/// SS-W9 (revised SG3): a fixture item with an input account cannot be
/// deleted by this binary (denied): a failed delete, kept as an orphan; a
/// fixture item with the account `other` is unrecognized, never read or
/// deleted; both count; a put still succeeds.
#[test]
fn ss_w9_denied_delete_and_unrecognized_items() {
    let mut scratch = Scratch::new();
    scratch.fixture(&format!("input.{}", hex(&id(1))));
    scratch.fixture("other");
    let gate = Gate::default();
    let store = scratch.gated(&gate);
    let report = sweep(&store, []);
    assert_eq!(
        (
            report.deleted,
            report.failed,
            report.unrecognized,
            report.outcome
        ),
        (0, 1, 1, Ok(()))
    );
    let observation = store.observation();
    assert_eq!(observation.unrecognized_count, 1);
    assert_eq!(observation.item_count, Some(2));
    // One list and one delete (of the input item); no read of either item.
    assert_eq!(gate.calls(), vec![Call::List, Call::Delete]);
    assert_eq!(put(&store, 2, small()), PutReply::Stored);
    assert_eq!(store.observation().item_count, Some(3));
    stop(store);
    assert_eq!(scratch.listed(), 3);
    scratch.delete_keychain();
}

/// SS-W10: 258 items, none live: the first listing returns 257 and is
/// truncated; the sweep lists again and ends with zero items.
#[test]
fn ss_w10_truncated_listing_is_swept_in_passes() {
    let mut scratch = Scratch::new();
    scratch.fill(1..=258);
    let listing = scratch
        .keychain()
        .list(&scratch.installation)
        .expect("list");
    assert_eq!(
        (listing.inputs.len(), listing.truncated),
        (LIST_BOUND, true)
    );
    let gate = Gate::default();
    let store = scratch.gated(&gate);
    let report = sweep(&store, []);
    assert_eq!(
        (
            report.deleted,
            report.listings,
            report.truncated,
            report.outcome
        ),
        (258, 2, false, Ok(()))
    );
    assert_eq!(gate.calls().iter().filter(|c| **c == Call::List).count(), 2);
    assert_eq!(store.observation().item_count, Some(0));
    stop(store);
    assert_eq!(scratch.listed(), 0);
    scratch.delete_keychain();
}
