//! PES-M11 and PES-M12: latency at 64 concurrent requests on 8 workers
//! (design: "Latency harness"). Usage: `bench cedar-b|cedar-a|rego`.
//! Diagnostic variants, not designed measurements: `cedar-sliced` (pattern B
//! pre-partitioned by action), and `-owned` suffixes, where each worker loads
//! its own engine instead of sharing one.
//! Every loop ends by iteration count; nothing ends by wall-clock time.

use asura_policy_engine_spike::cedar_engine::{CedarEngine, Pattern};
use asura_policy_engine_spike::model::Request;
use asura_policy_engine_spike::rego_engine::RegoEngine;
use asura_policy_engine_spike::stats::percentile;
use asura_policy_engine_spike::{all_requests, corpus, engines};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

const WORKERS: usize = 8;
const QUEUE: usize = 64;
const CLIENTS: usize = 64;
const WARMUP: usize = 1_000;
const MEASURED: usize = 10_000;

struct Job {
    index: usize,
    record: bool,
    reply: SyncSender<(bool, Option<u64>)>,
}

enum Evaluator {
    Cedar(Arc<CedarEngine>, Pattern),
    Rego(Box<RegoEngine>),
}

impl Evaluator {
    fn decide(&mut self, req: &Request) -> bool {
        match self {
            Evaluator::Cedar(e, p) => e.decide(req, *p).allow,
            Evaluator::Rego(e) => e.decide(req).allow,
        }
    }
}

fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "cedar-b".into());
    let scale = corpus::scale();
    let requests = Arc::new(all_requests(&scale));
    let (cedar, rego) = engines(&scale);
    let cedar = Arc::new(cedar);
    let owned = || {
        Arc::new(
            CedarEngine::load(
                &corpus::schema_text(),
                &corpus::all_cedar(&scale),
                &scale.store,
            )
            .expect("Cedar loads"),
        )
    };
    let make = |_: usize| -> Evaluator {
        match which.as_str() {
            "cedar-b" => Evaluator::Cedar(cedar.clone(), Pattern::B),
            "cedar-a" => Evaluator::Cedar(cedar.clone(), Pattern::A),
            "cedar-sliced" => Evaluator::Cedar(cedar.clone(), Pattern::BSliced),
            "cedar-b-owned" => Evaluator::Cedar(owned(), Pattern::B),
            "cedar-sliced-owned" => Evaluator::Cedar(owned(), Pattern::BSliced),
            "rego" => Evaluator::Rego(Box::new(rego.clone())),
            "rego-owned" => Evaluator::Rego(Box::new(
                RegoEngine::load(
                    &corpus::rego_lib(),
                    &corpus::rego_decision(),
                    &corpus::all_rego(&scale),
                    &scale.store,
                )
                .expect("Rego loads"),
            )),
            other => panic!("unknown engine {other}"),
        }
    };

    let (tx, rx) = mpsc::sync_channel::<Job>(QUEUE);
    let rx: Arc<Mutex<Receiver<Job>>> = Arc::new(Mutex::new(rx));
    let workers: Vec<_> = (0..WORKERS)
        .map(|w| {
            let rx = rx.clone();
            let requests = requests.clone();
            let mut eval = make(w);
            thread::spawn(move || {
                let mut samples = Vec::with_capacity(CLIENTS * MEASURED / WORKERS + 1024);
                loop {
                    let job = match rx.lock() {
                        Ok(guard) => match guard.recv() {
                            Ok(j) => j,
                            Err(_) => break, // queue closed and empty
                        },
                        Err(_) => break,
                    };
                    let start = Instant::now();
                    let allow = eval.decide(&requests[job.index]);
                    let ns = start.elapsed().as_nanos() as u64;
                    if job.record {
                        samples.push(ns);
                    }
                    if job.reply.send((allow, job.record.then_some(ns))).is_err() {
                        break;
                    }
                }
                samples
            })
        })
        .collect();

    let started = Instant::now();
    let clients: Vec<_> = (0..CLIENTS)
        .map(|c| {
            let tx = tx.clone();
            let n = requests.len();
            thread::spawn(move || {
                let (rtx, rrx) = mpsc::sync_channel(1);
                let mut e2e = Vec::with_capacity(MEASURED);
                let mut allows = 0usize;
                for k in 0..(WARMUP + MEASURED) {
                    let record = k >= WARMUP;
                    let submitted = Instant::now();
                    let job = Job {
                        index: (c * 997 + k) % n,
                        record,
                        reply: rtx.clone(),
                    };
                    if tx.send(job).is_err() {
                        break;
                    }
                    let Ok((allow, _)) = rrx.recv() else { break };
                    if record {
                        e2e.push(submitted.elapsed().as_nanos() as u64);
                        allows += usize::from(allow);
                    }
                }
                (e2e, allows)
            })
        })
        .collect();
    drop(tx);
    let mut e2e = Vec::new();
    let mut allows = 0;
    for c in clients {
        let (v, a) = c.join().expect("client");
        e2e.extend(v);
        allows += a;
    }
    let elapsed = started.elapsed();
    let mut eval: Vec<u64> = workers
        .into_iter()
        .flat_map(|w| w.join().expect("worker"))
        .collect();
    eval.sort_unstable();
    e2e.sort_unstable();
    let us = |v: u64| v as f64 / 1000.0;
    println!(
        "{{\"engine\":\"{which}\",\"workers\":{WORKERS},\"queue\":{QUEUE},\"clients\":{CLIENTS},\"measured\":{},\
         \"eval_us\":{{\"p50\":{:.2},\"p95\":{:.2},\"p99\":{:.2},\"max\":{:.2}}},\
         \"e2e_us\":{{\"p50\":{:.2},\"p95\":{:.2},\"p99\":{:.2},\"max\":{:.2}}},\
         \"throughput_per_s\":{:.0},\"allow_share\":{:.3}}}",
        e2e.len(),
        us(percentile(&eval, 50.0)),
        us(percentile(&eval, 95.0)),
        us(percentile(&eval, 99.0)),
        us(*eval.last().unwrap_or(&0)),
        us(percentile(&e2e, 50.0)),
        us(percentile(&e2e, 95.0)),
        us(percentile(&e2e, 99.0)),
        us(*e2e.last().unwrap_or(&0)),
        (CLIENTS * (WARMUP + MEASURED)) as f64 / elapsed.as_secs_f64(),
        allows as f64 / e2e.len().max(1) as f64,
    );
}
