//! The signed qualification run of the team Keychain store (SGQ6;
//! `docs/designs/release-signing.md#signed-qualification-run`).
//! `test-support` only; never part of `Asura.app`.
//!
//! The binary must run from a bundle signed with the test App ID, so its
//! access group is `<TeamID>.org.pidster.asura.test`. It refuses, with no
//! Keychain call, unless the access group check selects that group. Then
//! it drives the production backend (`Target::Default`) through the
//! production store worker: Q0 purge, Q1 empty sweep, Q2 put of the largest
//! payload and the exact-retry replies, Q3 read back, Q4 restart and sweep,
//! Q5 delete, absent and missing, Q6 observation, Q7 purge and an empty
//! group. Every wait ends by the worker's reply; there is no time bound.
//! The operator cancels with Ctrl-C; the next run purges first.
use super::{
    DeleteReply, GetReply, PutReply, SecretValue, Secrets, Store, SweepReport, Ticket, binding,
};
use asura_platform::keychain::Target;
use asura_platform::keychain::data_protection::qualification::{purge_test_group, test_group};
use std::process::ExitCode;
use zeroize::Zeroizing;

/// Exit status of a refusal: no Keychain call was made.
const REFUSED: u8 = 3;

fn bytes(len: usize, seed: u8) -> Zeroizing<Vec<u8>> {
    Zeroizing::new(
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31) ^ seed)
            .collect(),
    )
}

/// The largest payload of the item contract (8,307 bytes encoded).
fn largest(seed: u8) -> Secrets {
    Secrets {
        variables: (0..16u8)
            .map(|i| SecretValue {
                number: i + 1,
                value: bytes(512, seed ^ i),
            })
            .collect(),
        whole_command: None,
    }
}

struct Run {
    failed: u32,
}

impl Run {
    fn check(&mut self, case: &str, ok: bool, detail: impl std::fmt::Debug) {
        if ok {
            println!("PASS {case}");
        } else {
            self.failed += 1;
            println!("FAIL {case}: {detail:?}");
        }
    }
}

fn wait<T>(ticket: Result<Ticket<T>, super::Refused>) -> Option<T> {
    ticket.ok()?.wait()
}

/// The body of `secret_store_qualification`. Exit 0 when every case passes
/// and the test group ends empty, 1 on a failed case, 3 on a refusal.
pub fn qualification_main() -> ExitCode {
    let Some(group) = test_group() else {
        println!(
            "refused: this process's access group is not <TeamID>.org.pidster.asura.test (not_signed or another group); no Keychain call was made"
        );
        return ExitCode::from(REFUSED);
    };
    println!("group {group}");
    let mut run = Run { failed: 0 };
    let purged = purge_test_group();
    run.check("Q0 purge the test group first", purged.is_ok(), purged);

    let installation = asura_platform::random_id();
    let project = asura_platform::random_id();
    let conversation = asura_platform::random_id();
    let first = asura_platform::random_id();
    let second = asura_platform::random_id();
    let bind = |input: &[u8; 16]| binding(&installation, &project, &conversation, input, &[7; 32]);

    let store = match Store::new(Target::Default, installation, None) {
        Ok(store) => store,
        Err(error) => {
            println!("FAIL store start: {error}");
            return ExitCode::from(1);
        }
    };
    let report = wait(store.sweep(Default::default()));
    run.check(
        "Q1 sweep of an empty store lists zero items",
        matches!(
            report,
            Some(SweepReport {
                kept: 0,
                deleted: 0,
                failed: 0,
                outcome: Ok(()),
                ..
            })
        ) && store.observation().item_count == Some(0),
        (&report, store.observation()),
    );

    let put = wait(store.put(first, bind(&first), largest(1)));
    run.check(
        "Q2 put of a 55,413-byte payload",
        put == Some(PutReply::Stored),
        put,
    );
    let same = wait(store.put(first, bind(&first), largest(1)));
    run.check(
        "Q2 exact retry with equal values is exists_same",
        same == Some(PutReply::ExistsSame),
        same,
    );
    let different = wait(store.put(first, bind(&first), largest(2)));
    run.check(
        "Q2 retry with other values is exists_different",
        different == Some(PutReply::ExistsDifferent),
        different,
    );
    let read = wait(store.get(first, bind(&first)));
    run.check(
        "Q3 read returns the stored values exactly",
        read == Some(GetReply::Values(largest(1))),
        read.as_ref().map(std::mem::discriminant),
    );

    let put = wait(store.put(second, bind(&second), largest(3)));
    run.check("Q4 put a second item", put == Some(PutReply::Stored), put);
    // Restart: a new worker over the same installation.
    drop(store.close().join());
    let store = match Store::new(Target::Default, installation, None) {
        Ok(store) => store,
        Err(error) => {
            println!("FAIL store restart: {error}");
            return ExitCode::from(1);
        }
    };
    let report = wait(store.sweep([first].into_iter().collect()));
    run.check(
        "Q4 after restart the sweep keeps the live item and deletes the other",
        matches!(
            report,
            Some(SweepReport {
                kept: 1,
                deleted: 1,
                failed: 0,
                outcome: Ok(()),
                ..
            })
        ),
        report,
    );
    let read = wait(store.get(first, bind(&first)));
    run.check(
        "Q5 the restarted worker reads the live item",
        read == Some(GetReply::Values(largest(1))),
        read.as_ref().map(std::mem::discriminant),
    );
    let deleted = wait(store.delete(first));
    run.check("Q5 delete", deleted == Some(DeleteReply::Deleted), deleted);
    let absent = wait(store.delete(first));
    run.check(
        "Q5 second delete is absent",
        absent == Some(DeleteReply::Absent),
        absent,
    );
    let missing = wait(store.get(first, bind(&first)));
    run.check(
        "Q5 read after delete is missing",
        missing == Some(GetReply::Missing),
        missing.as_ref().map(std::mem::discriminant),
    );
    let observation = store.observation();
    run.check(
        "Q6 observation: zero items, status 0",
        observation.item_count == Some(0) && observation.last_status == 0,
        &observation,
    );
    drop(store.close().join());

    let purged = purge_test_group();
    run.check(
        "Q7 purge and list the test group: empty",
        purged.is_ok(),
        purged,
    );
    println!(
        "test-group-empty={}",
        if purged.is_ok() { "yes" } else { "no" }
    );
    if run.failed == 0 {
        println!("qualification passed");
        ExitCode::SUCCESS
    } else {
        println!("qualification failed: {} case(s)", run.failed);
        ExitCode::from(1)
    }
}
