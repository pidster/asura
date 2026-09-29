use super::*;
use std::sync::{Condvar, Mutex};

#[derive(Default)]
struct Gate {
    entered: AtomicBool,
    released: Mutex<bool>,
    ready: Condvar,
    records: Mutex<Vec<Record>>,
    fail: AtomicBool,
}
impl Gate {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.ready.notify_all();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct FakeSink(Arc<Gate>);
impl Sink for FakeSink {
    fn append(
        &mut self,
        records: &[Record],
        _: Instant,
        _: &AtomicBool,
    ) -> Result<(), record::Error> {
        self.0.entered.store(true, Ordering::Release);
        let mut released = self.0.released.lock().unwrap();
        while !*released {
            released = self.0.ready.wait(released).unwrap();
        }
        if self.0.fail.load(Ordering::Acquire) {
            return Err(record::Error::Unconfirmed);
        }
        self.0.records.lock().unwrap().extend_from_slice(records);
        Ok(())
    }
    fn maintain(&mut self, _: Instant, _: &AtomicBool) -> Result<(), record::Error> {
        Ok(())
    }
    fn sync(&mut self, _: Instant, _: &AtomicBool) -> Result<(), record::Error> {
        Ok(())
    }
}
fn changed() -> Event {
    Event::ConfigChanged {
        setting: record::Setting::Model,
        outcome: record::ConfigOutcome::Persisted,
    }
}
fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !predicate() {
        assert!(Instant::now() < deadline, "audit fixture deadline");
        thread::yield_now();
    }
}
fn fixture(settings: Settings) -> (Worker, Arc<Gate>, Release) {
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let sink = gate.clone();
    let (_, wake) = asura_platform::events::wake_pair().unwrap();
    let worker = Worker::launch([1; 16], "asura/test".into(), wake, move |_| {
        Ok(Bootstrap {
            settings,
            configuration: ConfigLoad::Defaults,
            sink: Box::new(FakeSink(sink)),
            recent: Vec::new(),
            older_omitted: false,
            lost_partial: false,
        })
    })
    .unwrap();
    (worker, gate, release)
}
fn settle(worker: &mut Worker, gate: &Gate) {
    worker.close(Instant::now());
    gate.release();
    wait_until(|| {
        worker.poll(Instant::now());
        worker.settled()
    });
    assert!(worker.next_deadline(Instant::now()).is_none());
}
#[test]
fn stalled_append_is_invisible_bounded_and_retained_through_close() {
    let (mut worker, gate, _release) = fixture(Settings::default());
    wait_until(|| gate.entered.load(Ordering::Acquire));
    worker.poll(Instant::now());
    assert!(
        worker.snapshot(Instant::now()).records.is_empty(),
        "append has not synced"
    );
    let emitter = worker.emitter();
    for _ in 0..CAPACITY {
        assert_eq!(emitter.try_emit(changed()), Admission::Accepted);
    }
    assert_eq!(emitter.try_emit(changed()), Admission::Dropped);
    assert_eq!(
        worker.health(Instant::now() + CYCLE + CYCLE).state,
        HealthState::Stale
    );
    assert_eq!(worker.health(Instant::now()).dropped, 1);
    worker.close(Instant::now());
    assert!(!worker.settled());
    assert!(worker.settlement_expired(Instant::now() + DRAIN + DRAIN));
    assert_eq!(emitter.try_emit(changed()), Admission::Closed);
    settle(&mut worker, &gate);
}
#[test]
fn successful_sync_publishes_and_failure_does_not_replay() {
    for fail in [false, true] {
        let (mut worker, gate, _release) = fixture(Settings::default());
        gate.fail.store(fail, Ordering::Release);
        wait_until(|| gate.entered.load(Ordering::Acquire));
        worker.poll(Instant::now());
        assert!(worker.snapshot(Instant::now()).records.is_empty());
        gate.release();
        wait_until(|| {
            worker.poll(Instant::now());
            if fail {
                worker.settled()
            } else {
                !worker.snapshot(Instant::now()).records.is_empty()
            }
        });
        if fail {
            assert!(gate.records.lock().unwrap().is_empty());
            assert_eq!(
                worker.health(Instant::now()).state,
                HealthState::Unavailable
            );
            assert_eq!(worker.emitter().try_emit(changed()), Admission::Unavailable);
        } else {
            let snapshot = worker.snapshot(Instant::now());
            assert_eq!(snapshot.records[0].sequence, 1);
            assert!(matches!(
                snapshot.records[0].event,
                Event::ServiceStarted { .. }
            ));
        }
        settle(&mut worker, &gate);
    }
}
#[test]
fn disabled_and_invalid_configuration_do_not_write() {
    let (mut worker, gate, _release) = fixture(Settings {
        enabled: false,
        ..Settings::default()
    });
    wait_until(|| {
        worker.poll(Instant::now());
        worker.settled()
    });
    assert_eq!(worker.health(Instant::now()).state, HealthState::Disabled);
    assert_eq!(worker.emitter().try_emit(changed()), Admission::Disabled);
    assert!(!gate.entered.load(Ordering::Acquire));
    let (_, wake) = asura_platform::events::wake_pair().unwrap();
    let mut invalid = Worker::launch([1; 16], "asura/test".into(), wake, |_| {
        Err(StartError::Config)
    })
    .unwrap();
    wait_until(|| {
        invalid.poll(Instant::now());
        invalid.settled()
    });
    assert_eq!(
        invalid.health(Instant::now()).reason,
        Some(HealthReason::ConfigInvalid)
    );
    assert_eq!(invalid.health(Instant::now()).keep_files, None);
}
#[test]
fn shared_producer_records_exact_loss_interval_before_next_event() {
    let (mut worker, gate, _release) = fixture(Settings::default());
    wait_until(|| gate.entered.load(Ordering::Acquire));
    let emitter = worker.emitter();
    for _ in 0..CAPACITY {
        assert_eq!(emitter.try_emit(changed()), Admission::Accepted);
    }
    assert_eq!(emitter.try_emit(changed()), Admission::Dropped);
    assert_eq!(emitter.clone().try_emit(changed()), Admission::Dropped);
    gate.release();
    wait_until(|| gate.records.lock().unwrap().len() > CAPACITY);
    assert_eq!(emitter.try_emit(changed()), Admission::Accepted);
    wait_until(|| {
        gate.records
            .lock()
            .unwrap()
            .iter()
            .any(|r| matches!(r.event, Event::JournalGap { .. }))
    });
    settle(&mut worker, &gate);
    let records = gate.records.lock().unwrap();
    let gap = records
        .iter()
        .find(|r| matches!(r.event, Event::JournalGap { .. }))
        .unwrap();
    assert_eq!(gap.sequence, 259);
    assert!(matches!(
        gap.event,
        Event::JournalGap {
            count: 2,
            first_sequence: 258,
            last_sequence: 259,
            reason: HealthReason::QueueFull
        }
    ));
    assert!(
        records
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );
    assert!(worker.snapshot(Instant::now()).records.len() <= CAPACITY);
    assert!(worker.health(Instant::now()).older_omitted);
}
#[test]
fn sequence_exhaustion_and_tiny_file_limit_are_visible_without_oversize_write() {
    let (mut worker, gate, _release) = fixture(Settings {
        max_file_bytes: 1,
        ..Settings::default()
    });
    worker.emitter.admission.borrow_mut().next = u64::MAX;
    assert_eq!(
        worker.emitter().try_emit(changed()),
        Admission::SequenceExhausted
    );
    assert_eq!(
        worker.health(Instant::now()).reason,
        Some(HealthReason::SequenceExhausted)
    );
    assert_eq!(
        worker.health(Instant::now() + CYCLE + CYCLE).state,
        HealthState::Unavailable
    );
    assert_eq!(
        worker.health(Instant::now() + CYCLE + CYCLE).reason,
        Some(HealthReason::SequenceExhausted)
    );
    settle(&mut worker, &gate);
    assert!(gate.records.lock().unwrap().is_empty());
    assert!(worker.health(Instant::now()).dropped > 0);
}

#[test]
fn bootstrap_gate_requires_actual_return_not_expired_or_unavailable_health() {
    for panic_after_release in [false, true] {
        let (release, blocked) = mpsc::sync_channel(1);
        let (_, wake) = asura_platform::events::wake_pair().unwrap();
        let mut worker = Worker::launch([1; 16], "asura/test".into(), wake, move |_| {
            blocked.recv_timeout(Duration::from_secs(2)).unwrap();
            if panic_after_release {
                panic!("bootstrap fixture unwind");
            }
            Err(StartError::Config)
        })
        .unwrap();
        assert!(!worker.bootstrap_settled());
        assert_eq!(
            worker.health(Instant::now() + Duration::from_secs(3)).state,
            HealthState::Stale
        );
        let mut failed = Snapshot::starting();
        failed.health.state = HealthState::Unavailable;
        worker.emitter.shared.publish(&failed);
        worker.poll(Instant::now());
        assert_eq!(
            worker.health(Instant::now()).state,
            HealthState::Unavailable
        );
        assert!(!worker.bootstrap_settled());
        release.send(()).unwrap();
        wait_until(|| {
            worker.poll(Instant::now());
            worker.settled()
        });
        assert!(worker.bootstrap_settled());
    }
}
