//! Typed inspection through the real local service and embedded storage.
use super::*;
fn query(project: [u8; 16]) -> pb::SensorsInspect {
    pb::SensorsInspect {
        project_id: Some(project.to_vec()),
        revision: None,
        offset: Some(0),
        limit: Some(16),
    }
}
fn loaded(fixture: &Fixture, project: [u8; 16]) -> Result<pb::SensorsReply> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let reply = fixture.attach()?.sensors(query(project))?;
        if reply.error.is_none() {
            return Ok(reply);
        }
        if !matches!(
            reply.error.as_deref(),
            Some("sensor_loading" | "conversation_busy")
        ) || Instant::now() >= deadline
        {
            return Err(format!("sensor load: {:?}", reply.error).into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}
pub(super) fn run() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.start()?;
    wait_ready(&fixture, None)?;
    assert_eq!(
        fixture.attach()?.sensors(query([99; 16]))?.error.as_deref(),
        Some("project_unknown")
    );
    let dir = fixture.home.join("project");
    fs::create_dir(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let registered = fixture.attach()?.register_project(pb::ProjectRegister {
        request_id: Some(asura_platform::random_id().to_vec()),
        location: Some(dir.to_str().unwrap().into()),
    })?;
    assert!(registered.error.is_none(), "{:?}", registered.error);
    let project: [u8; 16] = registered.project_id.unwrap().try_into().unwrap();
    let initial = loaded(&fixture, project)?;
    assert_eq!(initial.total_observations, Some(0));
    assert_eq!(initial.revision, Some(0));
    fixture.stop()?;
    seed(&fixture, project)?;
    fixture.start()?;
    wait_ready(&fixture, None)?;
    let committed = loaded(&fixture, project)?;
    assert_eq!(committed.observations.len(), 16);
    assert_eq!(committed.next_offset, Some(16));
    let continuation = fixture.attach()?.sensors(pb::SensorsInspect {
        offset: Some(16),
        revision: committed.revision,
        ..query(project)
    })?;
    assert_eq!(continuation.observations.len(), 2);
    assert_eq!(continuation.next_offset, None);
    assert_eq!(
        fixture.attach()?.inspect()?.lifecycle,
        pb::Lifecycle::Serving as i32
    );
    assert!(
        committed
            .proposals
            .iter()
            .all(|p| p.state == Some(1) && p.reason == Some(1))
    );
    let bad = pb::SensorsInspect {
        revision: Some(u64::MAX),
        ..query(project)
    };
    assert_eq!(
        fixture.attach()?.sensors(bad)?.error.as_deref(),
        Some("revision_conflict")
    );
    let ids: Vec<_> = committed
        .observations
        .iter()
        .map(|o| o.id.clone())
        .collect();
    fixture.stop()?;
    fixture.start()?;
    wait_ready(&fixture, None)?;
    let recovered = loaded(&fixture, project)?;
    assert!(
        ids.iter()
            .all(|id| recovered.observations.iter().any(|o| &o.id == id))
    );
    assert!(recovered.revision >= committed.revision);
    fixture.stop()?;
    println!(
        "PASS sensor inspection: scoped committed evidence, held proposals, controls and restart"
    );
    Ok(())
}

// Seed through the canonical writer only while the fixture service is stopped.
fn seed(fixture: &Fixture, project: [u8; 16]) -> Result<()> {
    use asura_storage::{authority::writer, sensors as record};
    let mut owner = writer::WriterHandle::start(fixture.runtime()?)
        .map_err(|e| format!("writer start: {e:?}"))?;
    let result: Result<()> = (|| {
        fn wait(mut ticket: writer::Ticket) -> Result<writer::Reply> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(reply) = ticket.poll() {
                    return reply.map_err(|e| format!("writer reply: {e:?}").into());
                }
                if Instant::now() >= deadline {
                    return Err("writer fixture deadline".into());
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
        wait(
            owner
                .try_submit(writer::Command::Open)
                .map_err(|e| format!("writer open: {e:?}"))?,
        )?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;
        let mut state = record::ProjectState::empty(project);
        state.revision = 1;
        state.write_id = [44; 16];
        state.last_received_ms = now;
        for sequence in 1..=18 {
            state.observations.push(record::Observation {
                id: record::observation_id(project, record::Source::Activity, [55; 16], sequence),
                project,
                source: record::Source::Activity,
                source_epoch: [55; 16],
                sequence,
                observed_ms: now,
                received_ms: now,
                expires_ms: now + 300_000,
                causal_root: Some([66; 16]),
                background: false,
                causal_hops: 0,
                payload: record::Payload::Activity {
                    operation: [66; 16],
                    generation: 1,
                    phase: record::Phase::Settled,
                    active_foreground: 0,
                },
            });
        }
        state.proposals.push(record::Proposal {
            id: [77; 16],
            purpose: record::Purpose::Consolidation,
            state: record::ProposalState::Held,
            reason: record::HoldReason::SensorAdmissionUnavailable,
            observations: state.observations.iter().map(|o| o.id).collect(),
            omitted: 0,
            created_ms: now,
            expires_ms: now + 300_000,
        });
        wait(
            owner
                .try_submit(writer::Command::SensorStore {
                    expected_revision: 0,
                    state,
                })
                .map_err(|e| format!("sensor store: {e:?}"))?,
        )?;
        Ok(())
    })();
    owner.close();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !owner.settled() {
        if Instant::now() >= deadline {
            return Err(format!("writer fixture cleanup deadline; original={result:?}").into());
        }
        thread::sleep(Duration::from_millis(1));
    }
    result
}
