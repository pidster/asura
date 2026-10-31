//! SS1-S and SG3 unit cases over a scripted port. No case makes a Keychain
//! call. Failure replies carry the session state the worker reads; within
//! one test process it is the same on every read, so cases compare it with
//! [`ev`].
use super::codec::{self, CodecError, decode, encode};
use super::*;
use asura_platform::keychain::{
    Added, Failure, Found, ItemKey, LIST_BOUND, Listing, MAX_ITEM_BYTES, Removed,
};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::sync::Condvar;
use zeroize::Zeroizing;

const INSTALLATION: [u8; 16] = [0x11; 16];
const BINDING: [u8; 32] = [0x22; 32];
const KEY: [u8; 32] = [0x33; 32];
/// The status an unscripted port call returns, so a test sees it.
const UNSCRIPTED: i32 = -1;

fn id(n: u16) -> [u8; 16] {
    let mut id = [0u8; 16];
    id[14..].copy_from_slice(&n.to_be_bytes());
    id
}

fn bytes(len: usize, seed: u8) -> Zeroizing<Vec<u8>> {
    Zeroizing::new(
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31) ^ seed)
            .collect(),
    )
}

fn variables(sizes: &[usize]) -> Vec<SecretValue> {
    sizes
        .iter()
        .enumerate()
        .map(|(i, &len)| SecretValue {
            number: i as u8 + 1,
            value: bytes(len, i as u8),
        })
        .collect()
}

/// The largest payload: 16 variables of 8,192 bytes in total (ENV1
/// removed the launch environment).
pub(super) fn largest() -> Secrets {
    Secrets {
        variables: variables(&[512; 16]),
        whole_command: None,
    }
}

fn small(seed: u8) -> Secrets {
    Secrets {
        variables: vec![SecretValue {
            number: 1,
            value: bytes(8, seed),
        }],
        ..Secrets::default()
    }
}

// ---- Scripted port -------------------------------------------------------

#[derive(Default)]
struct ScriptState {
    adds: VecDeque<Result<Added, Failure>>,
    gets: VecDeque<Result<Found, Failure>>,
    deletes: VecDeque<Result<Removed, Failure>>,
    lists: VecDeque<Result<Listing, Failure>>,
    log: Vec<(&'static str, Option<[u8; 16]>)>,
    held: bool,
    blocked: usize,
}

/// Returns queued platform results and records every call.
#[derive(Clone, Default)]
struct Script(Arc<(Mutex<ScriptState>, Condvar)>);

impl Script {
    fn with(&self, edit: impl FnOnce(&mut ScriptState)) -> &Self {
        edit(&mut self.0.0.lock().unwrap());
        self
    }
    fn add(&self, result: Result<Added, Failure>) -> &Self {
        self.with(|s| s.adds.push_back(result))
    }
    fn get(&self, result: Result<Found, Failure>) -> &Self {
        self.with(|s| s.gets.push_back(result))
    }
    fn delete(&self, result: Result<Removed, Failure>) -> &Self {
        self.with(|s| s.deletes.push_back(result))
    }
    fn list(&self, result: Result<Listing, Failure>) -> &Self {
        self.with(|s| s.lists.push_back(result))
    }
    fn log(&self) -> Vec<(&'static str, Option<[u8; 16]>)> {
        self.0.0.lock().unwrap().log.clone()
    }
    fn calls(&self, name: &str) -> usize {
        self.log().iter().filter(|(call, _)| *call == name).count()
    }
    fn hold(&self) {
        self.with(|s| s.held = true);
    }
    fn release(&self) {
        self.with(|s| s.held = false);
        self.0.1.notify_all();
    }
    fn wait_blocked(&self) {
        let mut state = self.0.0.lock().unwrap();
        while state.blocked == 0 {
            state = self.0.1.wait(state).unwrap();
        }
    }
    fn enter<T>(
        &self,
        name: &'static str,
        input: Option<[u8; 16]>,
        pick: impl FnOnce(&mut ScriptState) -> Option<Result<T, Failure>>,
    ) -> Result<T, Failure> {
        let mut state = self.0.0.lock().unwrap();
        state.log.push((name, input));
        state.blocked += 1;
        self.0.1.notify_all();
        while state.held {
            state = self.0.1.wait(state).unwrap();
        }
        state.blocked -= 1;
        pick(&mut state).unwrap_or(Err(Failure::Unavailable {
            status: UNSCRIPTED,
            item_call: true,
        }))
    }
}

impl port::KeychainPort for Script {
    fn add(&self, key: &ItemKey, _: &[u8]) -> Result<Added, Failure> {
        self.enter("add", Some(key.input), |s| s.adds.pop_front())
    }
    fn get(&self, key: &ItemKey) -> Result<Found, Failure> {
        self.enter("get", Some(key.input), |s| s.gets.pop_front())
    }
    fn delete(&self, key: &ItemKey) -> Result<Removed, Failure> {
        self.enter("delete", Some(key.input), |s| s.deletes.pop_front())
    }
    fn list(&self, installation: &[u8; 16]) -> Result<Listing, Failure> {
        assert_eq!(
            installation, &INSTALLATION,
            "only this installation is listed"
        );
        self.enter("list", None, |s| s.lists.pop_front())
    }
}

fn start(script: &Script) -> Store {
    let port = script.clone();
    Store::with_port(INSTALLATION, None, move || Ok(port.clone())).expect("worker")
}

fn listing(inputs: impl IntoIterator<Item = u16>, unrecognized: usize) -> Listing {
    let inputs: Vec<[u8; 16]> = inputs.into_iter().map(id).collect();
    Listing {
        truncated: inputs.len() + unrecognized == LIST_BOUND,
        inputs,
        unrecognized,
    }
}

fn data(secrets: &Secrets, binding: &[u8; 32]) -> Found {
    Found::Data(encode(binding, secrets, &KEY).expect("encode"))
}

/// The session state a failure reply carries in this process.
fn ev() -> Option<Session> {
    Some(Session::read())
}

/// The store failure of an `unavailable` port result.
fn store_unavailable(status: i32) -> StoreFailure {
    StoreFailure::Unavailable {
        status,
        reason: None,
        session: ev(),
    }
}

fn unavailable(status: i32) -> Failure {
    Failure::Unavailable {
        status,
        item_call: true,
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
        .sweep(live.into_iter().map(id).collect())
        .expect("admitted")
        .wait()
        .expect("reply")
}

// ---- SS-U1: payload codec --------------------------------------------------

/// SS-U1: every bound round trips exactly; one over each is rejected.
#[test]
fn ss_u1_codec_bounds() {
    let max = largest();
    let payload = encode(&BINDING, &max, &KEY).unwrap();
    assert_eq!(
        payload.len(),
        MAX_ITEM_BYTES,
        "largest payload is 8,307 bytes"
    );
    let decoded = decode(&payload, &BINDING).unwrap();
    assert_eq!(decoded.secrets, max);
    assert_eq!(*decoded.key, KEY);

    let whole = Secrets {
        whole_command: Some(bytes(MAX_WHOLE_COMMAND_BYTES, 9)),
        ..Secrets::default()
    };
    let payload = encode(&BINDING, &whole, &KEY).unwrap();
    assert_eq!(decode(&payload, &BINDING).unwrap().secrets, whole);
    assert_eq!(
        decode(
            &encode(&BINDING, &Secrets::default(), &KEY).unwrap(),
            &BINDING
        )
        .unwrap()
        .secrets,
        Secrets::default()
    );

    let mut invalid = Vec::new();
    // 17 variables; 8,193 bytes in total; number 0; numbers out of order.
    invalid.push(Secrets {
        variables: variables(&[1; 17]),
        ..Secrets::default()
    });
    let mut over = variables(&[512; 16]);
    over[15].value = bytes(513, 1);
    invalid.push(Secrets {
        variables: over,
        ..Secrets::default()
    });
    let mut zero = variables(&[1]);
    zero[0].number = 0;
    invalid.push(Secrets {
        variables: zero,
        ..Secrets::default()
    });
    let mut order = variables(&[1, 1]);
    order[1].number = 1;
    invalid.push(Secrets {
        variables: order,
        ..Secrets::default()
    });
    // Whole command of 8,193 bytes, empty, or beside numbered variables.
    for command in [bytes(MAX_WHOLE_COMMAND_BYTES + 1, 0), bytes(0, 0)] {
        invalid.push(Secrets {
            whole_command: Some(command),
            ..Secrets::default()
        });
    }
    invalid.push(Secrets {
        variables: variables(&[1]),
        whole_command: Some(bytes(1, 0)),
    });
    for secrets in &invalid {
        assert_eq!(
            encode(&BINDING, secrets, &KEY).map(drop),
            Err(CodecError::Bound),
            "{secrets:?}"
        );
    }

    // Decode rejects what encode would never write.
    let good = encode(&BINDING, &small(1), &KEY).unwrap();
    let mut version = good.to_vec();
    version[0] = 2;
    assert_eq!(decode(&version, &BINDING).err(), Some(CodecError::Version));
    let mut trailing = good.to_vec();
    trailing.push(0);
    assert_eq!(
        decode(&trailing, &BINDING).err(),
        Some(CodecError::Trailing)
    );
    assert_eq!(
        decode(&good[..good.len() - 1], &BINDING).err(),
        Some(CodecError::Truncated)
    );
    let mut flags = good.to_vec();
    flags[33] = 0x4;
    assert_eq!(decode(&flags, &BINDING).err(), Some(CodecError::Bound));
    // Flag bit 1, the launch environment that ENV1 removed, is refused.
    flags[33] = 0x2;
    assert_eq!(decode(&flags, &BINDING).err(), Some(CodecError::Bound));
    let mut count = good.to_vec();
    count[34] = 17;
    assert_eq!(decode(&count, &BINDING).err(), Some(CodecError::Bound));
    // A variable length over the 8,192-byte total is refused before a copy.
    let mut length = good.to_vec();
    length[36..38].copy_from_slice(&8_193u16.to_be_bytes());
    assert_eq!(decode(&length, &BINDING).err(), Some(CodecError::Bound));
    let oversize = vec![VERSION; MAX_ITEM_BYTES + 1];
    assert_eq!(
        decode(&oversize, &BINDING).err(),
        Some(CodecError::Oversize)
    );
}

/// The debug form of values shows counts only.
#[test]
fn secrets_debug_shows_no_value() {
    let text = format!("{:?}", small(0x5a));
    assert_eq!(text, "Secrets { variables: 1, whole_command: false }");
}

// ---- SS-U2: binding --------------------------------------------------------

/// SS-U2: the binding digest follows its definition; an item read under
/// another input or installation is `mismatch`.
#[test]
fn ss_u2_binding_vectors_and_mismatch() {
    let (installation, project, conversation, input) = ([1; 16], [2; 16], [3; 16], [4; 16]);
    let request = [5u8; 32];
    let mut hash = Sha256::new();
    hash.update(b"asura.secret-item\0");
    for part in [&installation, &project, &conversation, &input] {
        hash.update(part);
    }
    hash.update(request);
    let expected: [u8; 32] = hash.finalize().into();
    let bound = binding(&installation, &project, &conversation, &input, &request);
    assert_eq!(bound, expected);
    for other in [
        binding(&[9; 16], &project, &conversation, &input, &request),
        binding(&installation, &[9; 16], &conversation, &input, &request),
        binding(&installation, &project, &[9; 16], &input, &request),
        binding(&installation, &project, &conversation, &[9; 16], &request),
        binding(&installation, &project, &conversation, &input, &[9; 32]),
    ] {
        assert_ne!(other, bound);
    }

    let other_input = binding(&installation, &project, &conversation, &[9; 16], &request);
    let other_installation = binding(&[9; 16], &project, &conversation, &input, &request);
    let script = Script::default();
    script
        .get(Ok(data(&small(1), &other_input)))
        .get(Ok(data(&small(1), &other_installation)))
        .get(Ok(data(&small(1), &bound)));
    let store = start(&script);
    for _ in 0..2 {
        let reply = store.get(input, bound).unwrap().wait().unwrap();
        assert_eq!(reply, GetReply::Mismatch);
    }
    let reply = store.get(input, bound).unwrap().wait().unwrap();
    assert_eq!(reply, GetReply::Values(small(1)));
}

// ---- SS-U10: worker reply table -------------------------------------------

/// SS-U10: each platform result of the worker reply table, including
/// `Added::Duplicate` followed by each `get` result, and `Failure::Invalid`.
#[test]
fn ss_u10_reply_table() {
    let failures = [
        (
            Failure::Locked { status: -25_308 },
            StoreFailure::Locked {
                status: -25_308,
                session: ev(),
            },
        ),
        (
            Failure::Denied { status: -25_244 },
            StoreFailure::Denied {
                status: -25_244,
                session: ev(),
            },
        ),
        (unavailable(-25_294), store_unavailable(-25_294)),
        (
            Failure::Invalid,
            StoreFailure::Unavailable {
                status: 0,
                reason: Some(INVALID_REQUEST),
                session: None,
            },
        ),
    ];
    let script = Script::default();
    script.list(Ok(Listing::default()));
    let store = start(&script);

    // add
    script.add(Ok(Added::Stored));
    assert_eq!(put(&store, 1, small(1)), PutReply::Stored);
    for (failure, reply) in failures {
        script.add(Err(failure));
        assert_eq!(put(&store, 2, small(1)), PutReply::Failed(reply));
    }
    // add Duplicate, then each get result.
    let duplicates = [
        (Ok(data(&small(1), &BINDING)), PutReply::ExistsSame),
        (Ok(data(&small(2), &BINDING)), PutReply::ExistsDifferent),
        (Ok(data(&small(1), &[0x44; 32])), PutReply::ExistsDifferent),
        (
            Ok(Found::Missing),
            PutReply::Failed(store_unavailable(-25_300)),
        ),
    ];
    for (found, reply) in duplicates {
        script.add(Ok(Added::Duplicate)).get(found);
        assert_eq!(put(&store, 3, small(1)), reply);
    }
    for (failure, reply) in failures {
        script.add(Ok(Added::Duplicate)).get(Err(failure));
        assert_eq!(put(&store, 3, small(1)), PutReply::Failed(reply));
    }

    // get
    script.get(Ok(data(&largest(), &BINDING)));
    assert_eq!(get(&store, 4), GetReply::Values(largest()));
    script.get(Ok(Found::Missing));
    assert_eq!(get(&store, 4), GetReply::Missing);
    // The codec owns the read bound: 55,414 bytes is `mismatch`.
    script.get(Ok(Found::Data(Zeroizing::new(vec![
        VERSION;
        MAX_ITEM_BYTES + 1
    ]))));
    assert_eq!(get(&store, 4), GetReply::Mismatch);
    for (failure, reply) in failures {
        script.get(Err(failure));
        assert_eq!(get(&store, 4), GetReply::Failed(reply));
    }

    // delete
    script
        .delete(Ok(Removed::Deleted))
        .delete(Ok(Removed::Absent));
    assert_eq!(delete(&store, 5), DeleteReply::Deleted);
    assert_eq!(delete(&store, 5), DeleteReply::Absent);
    for (failure, reply) in failures {
        script.delete(Err(failure));
        assert_eq!(delete(&store, 6), DeleteReply::Failed(reply));
    }
    // An invalid request never reaches the port.
    let before = script.log().len();
    let invalid = Secrets {
        variables: variables(&[1; 17]),
        ..Secrets::default()
    };
    assert_eq!(
        put(&store, 7, invalid),
        PutReply::Failed(StoreFailure::Unavailable {
            status: 0,
            reason: Some(INVALID_REQUEST),
            session: None,
        })
    );
    assert_eq!(script.log().len(), before);
    assert_eq!(script.calls("list"), 1, "one listing set the count");
    drop(store.close().join());
}

/// `last_status` holds the last unavailable status until a call succeeds.
#[test]
fn last_status_follows_unavailable() {
    let script = Script::default();
    script
        .delete(Err(unavailable(-25_294)))
        .delete(Ok(Removed::Absent));
    let store = start(&script);
    delete(&store, 1);
    assert_eq!(store.observation().last_status, 25_294);
    delete(&store, 1);
    assert_eq!(store.observation().last_status, 0);
}

// ---- SS-U5: queue ----------------------------------------------------------

/// SS-U5: with the worker held inside a call, 16 requests are outstanding;
/// the 17th and a settlement delete get `busy`. All 16 are answered after
/// the release.
#[test]
fn ss_u5_queue_bound() {
    let script = Script::default();
    for _ in 0..QUEUE_BOUND {
        script.get(Ok(Found::Missing));
    }
    script.hold();
    let store = start(&script);
    let mut tickets = vec![store.get(id(1), BINDING).unwrap()];
    script.wait_blocked();
    for n in 2..=QUEUE_BOUND as u16 {
        tickets.push(store.get(id(n), BINDING).unwrap());
    }
    assert!(!store.has_room());
    assert_eq!(store.get(id(17), BINDING).err(), Some(Refused::Busy));
    assert_eq!(store.delete(id(1)).err(), Some(Refused::Busy));
    assert_eq!(
        store.put(id(18), BINDING, small(1)).err(),
        Some(Refused::Busy)
    );
    assert_eq!(tickets[0].poll(), Polled::Pending);
    script.release();
    for ticket in tickets {
        assert_eq!(ticket.wait(), Some(GetReply::Missing));
    }
    assert!(store.has_room());
    assert_eq!(script.calls("delete"), 0, "the busy delete never ran");
}

/// A closed worker still answers the requests already queued.
#[test]
fn closed_worker_finishes_queued_requests() {
    let script = Script::default();
    script.get(Ok(Found::Missing));
    let store = start(&script);
    let ticket = store.get(id(1), BINDING).unwrap();
    store.close().join().unwrap();
    assert_eq!(ticket.poll(), Polled::Ready(GetReply::Missing));
}

// ---- SS-U6 and SS-U11: sweep -----------------------------------------------

/// SS-U6: the store keeps every input of the live set (queued, held,
/// waiting, dispatched) and deletes every other input (settled, dropped,
/// unknown). The replay-state classification is the owner's (P3-E).
#[test]
fn ss_u6_sweep_keeps_live_inputs() {
    let script = Script::default();
    script.list(Ok(listing(1..=7, 0)));
    for _ in 5..=7 {
        script.delete(Ok(Removed::Deleted));
    }
    let store = start(&script);
    let report = sweep(&store, 1..=4);
    assert_eq!(
        (report.kept, report.deleted, report.listings, report.outcome),
        (4, 3, 1, Ok(()))
    );
    let deleted: Vec<_> = script
        .log()
        .into_iter()
        .filter(|(call, _)| *call == "delete")
        .map(|(_, input)| input.unwrap())
        .collect();
    assert_eq!(deleted, vec![id(5), id(6), id(7)]);
    assert_eq!(store.observation().item_count, Some(4));
}

/// SS-U11 (revised SG3): a denied delete is a failed delete that the next
/// sweep retries; unrecognized counts and truncated listings.
#[test]
fn ss_u11_sweep_counts_and_truncation() {
    let script = Script::default();
    let store = start(&script);

    // A denied delete (scratch backend) keeps the item as an orphan; a
    // later sweep deletes it.
    script
        .list(Ok(listing([1], 0)))
        .delete(Err(Failure::Denied { status: -25_244 }));
    let report = sweep(&store, []);
    assert_eq!((report.failed, report.deleted), (1, 0));
    assert_eq!(store.observation().item_count, Some(1), "the orphan counts");
    script
        .list(Ok(listing([1], 0)))
        .delete(Ok(Removed::Deleted));
    let report = sweep(&store, []);
    assert_eq!((report.failed, report.deleted), (0, 1));
    assert_eq!(store.observation().item_count, Some(0));

    // Two unrecognized accounts: counted, never read or deleted.
    let before = script.log().len();
    script.list(Ok(listing([], 2)));
    let report = sweep(&store, []);
    assert_eq!(report.unrecognized, 2);
    assert_eq!(script.log()[before..], [("list", None)]);
    assert_eq!(store.observation().unrecognized_count, 2);
    assert_eq!(store.observation().item_count, Some(2));

    // A truncated listing of 257 where the pass deletes 3, then 254.
    let live = 4..=255u16;
    script.list(Ok(listing(1..=255, 2)));
    for _ in 0..3 {
        script.delete(Ok(Removed::Deleted));
    }
    script.list(Ok(listing(live.clone(), 2)));
    let lists = script.calls("list");
    let report = sweep(&store, live.clone());
    assert_eq!(script.calls("list") - lists, 2, "second listing made");
    assert_eq!(
        (
            report.deleted,
            report.kept,
            report.truncated,
            report.listings
        ),
        (3, 252, false, 2)
    );
    assert_eq!(store.observation().item_count, Some(254));

    // A truncated listing where the pass deletes nothing: no second
    // listing; the count is 257 and `put` gives `full` without an add.
    script.list(Ok(listing(1..=255, 2)));
    let lists = script.calls("list");
    let report = sweep(&store, 1..=255);
    assert_eq!(script.calls("list") - lists, 1, "no second listing");
    assert!(report.truncated);
    assert_eq!(store.observation().item_count, Some(COUNT_TRUNCATED));
    assert_eq!(put(&store, 300, small(1)), PutReply::Full);
    assert_eq!(script.calls("add"), 0);
    // A delete does not lower a truncated count.
    script.delete(Ok(Removed::Deleted));
    delete(&store, 1);
    assert_eq!(store.observation().item_count, Some(COUNT_TRUNCATED));

    // A denied delete after settlement is a failed delete with evidence.
    script.delete(Err(Failure::Denied { status: -25_244 }));
    assert_eq!(
        delete(&store, 400),
        DeleteReply::Failed(StoreFailure::Denied {
            status: -25_244,
            session: ev(),
        })
    );
}

/// KC-U6: `not_signed` from the open is terminal: every later request
/// replies `unavailable/not_signed` (-34018) with evidence, and the worker
/// neither opens again nor reaches the port.
#[test]
fn not_signed_open_is_terminal() {
    let script = Script::default();
    let opens = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = opens.clone();
    let port = script.clone();
    let store = Store::with_port(INSTALLATION, None, move || {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if true {
            Err(Failure::NotSigned { item_call: false })
        } else {
            Ok(port.clone())
        }
    })
    .unwrap();
    let not_signed = StoreFailure::Unavailable {
        status: asura_platform::keychain::MISSING_ENTITLEMENT,
        reason: Some(NOT_SIGNED),
        session: ev(),
    };
    assert!(not_signed.not_signed());
    assert_eq!(get(&store, 1), GetReply::Failed(not_signed));
    assert_eq!(put(&store, 2, small(1)), PutReply::Failed(not_signed));
    assert_eq!(delete(&store, 3), DeleteReply::Failed(not_signed));
    let report = sweep(&store, []);
    assert_eq!(report.outcome, Err(not_signed));
    assert_eq!(
        opens.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one open"
    );
    assert!(script.log().is_empty(), "no port call");
    let observation = store.observation();
    assert_eq!(observation.last_status, 34_018);
    assert!(observation.sweep_pending);
}

/// KC-U6: `not_signed` from a call (-34018 after the open) is terminal too.
#[test]
fn not_signed_call_is_terminal() {
    let script = Script::default();
    script.get(Err(Failure::NotSigned { item_call: true }));
    let store = start(&script);
    let first = get(&store, 1);
    assert!(
        matches!(&first, GetReply::Failed(f) if f.not_signed() && f.status() == -34_018 && f.session().is_some()),
        "{first:?}"
    );
    assert_eq!(get(&store, 1), first);
    assert_eq!(script.calls("get"), 1, "no second port call");
}

/// KC-U5: every Keychain failure reply carries its raw status and the
/// session state; a defect carries neither session nor a real status.
#[test]
fn failure_replies_carry_field_evidence() {
    let script = Script::default();
    script
        .get(Err(Failure::Locked { status: -25_308 }))
        .delete(Err(unavailable(-25_291)));
    let store = start(&script);
    let GetReply::Failed(locked) = get(&store, 1) else {
        panic!("locked read");
    };
    assert_eq!((locked.code(), locked.status()), ("locked", -25_308));
    let session = locked.session().expect("session");
    assert!(session.boot_time_s.is_some() && session.audit_flags.is_some());
    let DeleteReply::Failed(unavailable) = delete(&store, 1) else {
        panic!("unavailable delete");
    };
    assert_eq!(unavailable.status(), -25_291);
    assert!(unavailable.session().is_some());
}

/// A sweep whose listing fails is pending, and the worker retries it before
/// its next request. Items stored since are kept by the retry.
#[test]
fn failed_sweep_is_retried_before_the_next_request() {
    let script = Script::default();
    script.list(Err(unavailable(-25_294)));
    let store = start(&script);
    let report = sweep(&store, [1]);
    assert_eq!(report.outcome, Err(store_unavailable(-25_294)));
    let observation = store.observation();
    assert!(observation.sweep_pending);
    assert_eq!(
        observation.item_count, None,
        "absent until a sweep completes"
    );
    assert_eq!(observation.last_status, 25_294);

    // The retry runs before this put; the put lists no more (count known).
    script
        .list(Ok(listing([1, 2], 0)))
        .delete(Ok(Removed::Deleted))
        .add(Ok(Added::Stored));
    assert_eq!(put(&store, 3, small(1)), PutReply::Stored);
    let observation = store.observation();
    assert!(!observation.sweep_pending);
    assert_eq!(observation.item_count, Some(2));
    assert_eq!(
        script.log(),
        vec![
            ("list", None),
            ("list", None),
            ("delete", Some(id(2))),
            ("add", Some(id(3))),
        ]
    );
    // A later sweep keeps the item this worker stored.
    script.list(Ok(listing([1, 3], 0)));
    let report = sweep(&store, [1]);
    assert_eq!((report.kept, report.deleted), (2, 0));
}

/// A failed open answers `unavailable` and is retried at the next request.
#[test]
fn failed_open_is_retried() {
    let script = Script::default();
    script.get(Ok(Found::Missing));
    let mut first = true;
    let port = script.clone();
    let store = Store::with_port(INSTALLATION, None, move || {
        if std::mem::take(&mut first) {
            Err(unavailable(-25_294))
        } else {
            Ok(port.clone())
        }
    })
    .unwrap();
    assert_eq!(get(&store, 1), GetReply::Failed(store_unavailable(-25_294)));
    assert_eq!(get(&store, 1), GetReply::Missing);
}

// ---- SS-U7 and SS-U8 -------------------------------------------------------

/// SS-U7 (service part): this crate's unit tests can never open the default
/// keychain; the request is refused before any Keychain call.
#[test]
fn ss_u7_unit_tests_never_open_the_default_keychain() {
    let store = Store::new(Target::Default, INSTALLATION, None).unwrap();
    assert_eq!(
        get(&store, 1),
        GetReply::Failed(StoreFailure::Unavailable {
            status: 0,
            reason: Some(INVALID_REQUEST),
            session: None,
        })
    );
}

/// SS-U8 (store part): an exact retry with equal values is `exists_same`,
/// with different values `exists_different`; neither writes a new item.
#[test]
fn ss_u8_exact_retry_compares_keyed_digests() {
    let script = Script::default();
    script
        .list(Ok(Listing::default()))
        .add(Ok(Added::Duplicate))
        .get(Ok(data(&largest(), &BINDING)))
        .add(Ok(Added::Duplicate))
        .get(Ok(data(&largest(), &BINDING)));
    let store = start(&script);
    assert_eq!(put(&store, 1, largest()), PutReply::ExistsSame);
    let mut changed = largest();
    changed.variables[3].value[0] ^= 1;
    assert_eq!(put(&store, 1, changed), PutReply::ExistsDifferent);
    // Equal values under different keys give different digests.
    let a = codec::value_digest(&[1; 32], &small(1));
    assert_ne!(a, codec::value_digest(&[2; 32], &small(1)));
    assert!(codec::digests_equal(
        &a,
        &codec::value_digest(&[1; 32], &small(1))
    ));
}

/// The locked recheck pacing: 1, 1, 2, 3, 5, 8 s and so on, at most 60 s.
#[test]
fn recheck_is_paced() {
    let mut recheck = Recheck::default();
    let delays: Vec<u64> = (0..13).map(|_| recheck.next_delay().as_secs()).collect();
    assert_eq!(delays, [1, 1, 2, 3, 5, 8, 13, 21, 34, 55, 60, 60, 60]);
}
