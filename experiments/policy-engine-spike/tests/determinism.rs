//! PES-M5: identical decisions over repetitions, threads and fresh engine
//! instances; and which nondeterministic built-ins remain reachable. The
//! cross-process part runs the `decisions` binary twice (see results/).

use asura_policy_engine_spike::cedar_engine::Pattern;
use asura_policy_engine_spike::corpus::{self, Rng};
use asura_policy_engine_spike::{all_requests, engines};
use std::thread;

const THREADS: usize = 8;
const REPS_PER_THREAD: usize = 13; // 104 repetitions in total

#[test]
fn decisions_are_identical_across_repetitions_threads_and_instances() {
    let scale = corpus::scale();
    let requests = all_requests(&scale);
    let (cedar, mut rego) = engines(&scale);
    let reference: Vec<[String; 3]> = requests
        .iter()
        .map(|r| {
            [
                cedar.decide(r, Pattern::B).render(),
                cedar.decide(r, Pattern::A).render(),
                rego.decide(r).render(),
            ]
        })
        .collect();
    let mismatches: usize = thread::scope(|s| {
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let (scale, requests, reference) = (&scale, &requests, &reference);
                s.spawn(move || {
                    // Fresh engine instances per thread.
                    let (cedar, mut rego) = engines(scale);
                    let mut rng = Rng::new(0xD00D + t as u64);
                    let mut order: Vec<usize> = (0..requests.len()).collect();
                    let mut bad = 0;
                    for _ in 0..REPS_PER_THREAD {
                        // Fisher-Yates shuffle with the deterministic generator.
                        for i in (1..order.len()).rev() {
                            order.swap(i, rng.below(i as u64 + 1) as usize);
                        }
                        for &i in &order {
                            let r = &requests[i];
                            let got = [
                                cedar.decide(r, Pattern::B).render(),
                                cedar.decide(r, Pattern::A).render(),
                                rego.decide(r).render(),
                            ];
                            if got != reference[i] {
                                bad += 1;
                            }
                        }
                    }
                    bad
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("worker")).sum()
    });
    println!(
        "requests={} repetitions={} evaluations={} mismatches={mismatches}",
        requests.len(),
        THREADS * REPS_PER_THREAD,
        requests.len() * THREADS * REPS_PER_THREAD * 3
    );
    assert_eq!(mismatches, 0);
}

#[test]
fn nondeterministic_rego_builtins_reachable_with_the_spike_features() {
    let scale = corpus::scale();
    let (_, rego) = engines(&scale);
    let probes = [
        ("time.now_ns", "x := time.now_ns()"),
        ("rand.intn", "x := rand.intn(\"seed\", 100)"),
        ("http.send", "x := http.send({})"),
        ("opa.runtime", "x := opa.runtime()"),
        ("uuid.rfc4122", "x := uuid.rfc4122(\"k\")"),
    ];
    let mut reachable = Vec::new();
    for (name, query) in probes {
        let mut e = rego.clone();
        let result = e.eval_query(query);
        let unknown = matches!(&result, Err(m) if m.contains("could not find function"));
        println!(
            "{name}: {}",
            match &result {
                Ok(v) => format!("evaluated: {v}"),
                Err(m) => format!(
                    "error: {}",
                    m.replace('\n', " ").chars().take(160).collect::<String>()
                ),
            }
        );
        if !unknown {
            reachable.push(name);
        }
    }
    println!("reachable nondeterministic built-ins: {reachable:?}");
    // Recorded measurement (design PES-M5); a change must be re-recorded.
    assert_eq!(reachable, REACHABLE_RECORDED);
}

const REACHABLE_RECORDED: [&str; 1] = ["rand.intn"];
