//! SJ-39: resident sensor states with more projects than the 64-state
//! memory bound (sensors.md, resident project states; OD-SJ-5 option A).
//! A real private journal and graph; the sensor wall clock is advanced on
//! the test thread only.
use super::*;
use std::{os::unix::fs::DirBuilderExt, time::Duration};

fn wait(writer: &writer::WriterHandle, command: writer::Command) -> writer::Reply {
    let mut ticket = writer.try_submit(command).unwrap();
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(reply) = ticket.poll() {
            return reply.unwrap();
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn append(
    writer: &writer::WriterHandle,
    record: journal::Record,
    admission: Option<writer::AdmissionFence>,
) {
    let revision = wait(writer, writer::Command::Open).replay.unwrap().revision;
    wait(
        writer,
        writer::Command::Append {
            expected_revision: revision,
            record,
            admission,
        },
    );
}
/// One completed turn in a new conversation of `project`.
fn turn(writer: &writer::WriterHandle, project: journal::Id, digest: journal::Hash) {
    let operation = asura_platform::random_id();
    append(
        writer,
        journal::Record::TurnAccepted(Box::new(journal::TurnAccepted {
            request: asura_platform::random_id(),
            digest: journal::request_digest(journal::Request::Submit {
                project,
                conversation: None,
                expected_generation: 0,
                prompt: "p",
            })
            .unwrap(),
            project,
            original_conversation: None,
            expected_generation: 0,
            conversation: asura_platform::random_id(),
            generation: 1,
            task: asura_platform::random_id(),
            operation,
            model: "system".into(),
            configuration_digest: digest,
            instructions_digest: [1; 32],
            input_digest: [2; 32],
            prior_operations: vec![],
            prompt: "p".into(),
            event_cursor: 1,
            composition: journal::Composition::default(),
        })),
        Some(writer::AdmissionFence {
            project,
            configuration_digest: digest,
        }),
    );
    append(
        writer,
        journal::Record::StartAuthorized(journal::StartAuthorized {
            operation,
            generation: 1,
            helper: asura_platform::random_id(),
            input_digest: [2; 32],
        }),
        None,
    );
    append(
        writer,
        journal::Record::TurnTerminal(journal::TurnTerminal {
            operation,
            generation: 1,
            kind: journal::TerminalKind::Complete,
            cause: journal::Cause::None,
            final_cursor: u64::MAX,
            usage_known: true,
            output_tokens: 1,
            text_offset: 0,
            text: "done".into(),
        }),
        None,
    );
}
/// Runs the owner's sensor work and writer replies until nothing is pending.
fn settle(owner: &mut Owner, now: Instant) {
    for _ in 0..10_000 {
        if let Some((ticket, _)) = owner.pending.as_mut() {
            let end = Instant::now() + Duration::from_secs(30);
            let result = loop {
                if let Some(result) = ticket.poll() {
                    break result;
                }
                assert!(Instant::now() < end);
                std::thread::sleep(Duration::from_millis(1));
            };
            let (_, job) = owner.pending.take().unwrap();
            owner.job_done(job, result, now);
            continue;
        }
        owner.drive_sensors(now);
        if owner.pending.is_none() {
            return;
        }
    }
    panic!("sensor work did not settle");
}

#[test]
fn sj39_resident_states_stay_bounded_and_no_activity_is_lost() {
    let _serial = crate::TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let mut fixture = tool_tests::Fixture::new();
    fixture.owner.active = None;
    let writer = writer::WriterHandle::start(fixture.owner.runtime.clone()).unwrap();
    wait(&writer, writer::Command::Initialize { request: [30; 16] });
    let mut projects = Vec::new();
    for n in 0..100u64 {
        let path = fixture.path.join(format!("p{n}"));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let project = wait(
            &writer,
            writer::Command::Register {
                request: asura_platform::random_id(),
                location: path.to_str().unwrap().into(),
            },
        )
        .project
        .unwrap()
        .registration
        .project;
        let digest = wait(
            &writer,
            writer::Command::Prepare {
                project,
                conversation: None,
                expected_generation: 0,
                prompt: "p".into(),
            },
        )
        .prepared
        .unwrap()
        .configuration_digest;
        turn(&writer, project, digest);
        projects.push(project);
    }
    let state = wait(&writer, writer::Command::Open).replay.unwrap();
    // Every project had activity; the recent set holds the newest 64.
    assert_eq!(state.projects.len(), journal::RECENT_PROJECTS);
    fixture.owner.replay = Some(state);
    fixture.owner.writer = Some(writer);
    fixture.owner.sensor_status(
        [7; 16],
        pb::InspectReply {
            lifecycle: Some(pb::Lifecycle::Serving as i32),
            installation: Some(pb::InstallationState::GraphReady as i32),
            reason: Some(pb::InstallationInspectionReason::GraphVerified as i32),
            ..Default::default()
        },
    );
    let owner = &mut fixture.owner;
    let mut now = Instant::now();
    settle(owner, now);
    let sensors = owner.sensor_owner.as_ref().unwrap();
    assert_eq!(
        sensors.len(),
        sensors::MAX_RESIDENT,
        "the start seed fills 64"
    );
    assert_eq!(owner.sensor_sequences.len(), sensors::MAX_RESIDENT);
    // Inspection of a project outside the resident set loads its state for
    // the reply only.
    let outside = *projects
        .iter()
        .find(|p| !owner.sensor_owner.as_ref().unwrap().resident(**p))
        .unwrap();
    owner.learn_project(&journal::ProjectRegistered {
        request: [0; 16],
        digest: [0; 32],
        project: outside,
        location: String::new(),
        device: 0,
        inode: 0,
        registry_revision: 1,
        visibility: 1,
    });
    owner.started = true;
    let request = pb::Envelope {
        service_epoch: Some(vec![1; 16]),
        attachment_id: Some(vec![2; 16]),
        request_counter: Some(1),
        body: Some(Body::SensorsInspect(pb::SensorsInspect {
            project_id: Some(outside.to_vec()),
            revision: None,
            offset: Some(0),
            limit: Some(16),
        })),
    };
    assert!(
        owner.request(request, now).is_none(),
        "loaded through the writer"
    );
    settle(owner, now);
    let reply = owner.out.pop_back().unwrap();
    let Some(Body::SensorsReply(inspected)) = reply.body else {
        panic!("sensors reply: {reply:?}");
    };
    assert!(inspected.error.is_none(), "{:?}", inspected.error);
    assert!(!owner.sensor_owner.as_ref().unwrap().resident(outside));
    // Activity during the run for the 36 projects outside the start seed:
    // with 64 unsettled states resident, it waits in the pending queue.
    let writer = owner.writer.take().unwrap();
    let mut later = 0;
    for project in &projects {
        if owner.sensor_owner.as_ref().unwrap().resident(*project) {
            continue;
        }
        let digest = wait(
            &writer,
            writer::Command::Prepare {
                project: *project,
                conversation: None,
                expected_generation: 0,
                prompt: "p".into(),
            },
        )
        .prepared
        .unwrap()
        .configuration_digest;
        turn(&writer, *project, digest);
        later += 1;
    }
    assert_eq!(later, 36);
    owner.replay = Some(wait(&writer, writer::Command::Open).replay.unwrap());
    owner.writer = Some(writer);
    settle(owner, now);
    let sensors = owner.sensor_owner.as_ref().unwrap();
    assert_eq!(sensors.len(), sensors::MAX_RESIDENT);
    assert!(sensors.deferred_len() > 0, "no settled state to evict yet");
    // Idle observations fire, proposals expire and their writes settle; a
    // settled state leaves memory and a non-resident project with recorded
    // activity loads in its place. Nothing is lost: every project's activity
    // is recorded once the time passes.
    let mut wall = 0u64;
    for _ in 0..60 {
        now += Duration::from_secs(330);
        wall += 330_000;
        WALL_OFFSET_MS.with(|offset| offset.set(wall));
        settle(owner, now);
        let sensors = owner.sensor_owner.as_ref().unwrap();
        assert!(sensors.len() <= sensors::MAX_RESIDENT);
        if owner.sensor_sequences.len() == projects.len() {
            break;
        }
    }
    let sensors = owner.sensor_owner.as_ref().unwrap();
    assert!(sensors.len() <= sensors::MAX_RESIDENT);
    assert_eq!(
        owner.sensor_sequences.len(),
        projects.len(),
        "every project's activity recorded; {} deferred, {} dropped",
        sensors.deferred_len(),
        sensors.deferred_dropped()
    );
    WALL_OFFSET_MS.with(|offset| offset.set(0));
    // Release the process-wide writer claim before the next test.
    let mut writer = fixture.owner.writer.take().unwrap();
    writer.close();
    let end = Instant::now() + Duration::from_secs(30);
    while !writer.settled() {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
}
