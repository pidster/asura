//! CC-I1 and CC-I7: composition over a real journal with more turns than the
//! former 32-turn and 64 KiB rules allowed, then exact reconstruction after replay.
use super::*;
use asura_storage::authority::{conversation as journal, writer};
use std::os::unix::fs::DirBuilderExt;
use std::time::{Duration, Instant};

struct Scratch(std::path::PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn wait(writer: &writer::WriterHandle, command: writer::Command) -> writer::Reply {
    let mut ticket = writer.try_submit(command).unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(reply) = ticket.poll() {
            return reply.unwrap();
        }
        assert!(Instant::now() < end, "writer deadline");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn close(writer: &mut writer::WriterHandle) {
    writer.close();
    let end = Instant::now() + Duration::from_secs(10);
    while !writer.settled() {
        assert!(Instant::now() < end, "writer close");
        std::thread::sleep(Duration::from_millis(1));
    }
}
/// Distinct identities per kind, so no fresh-ID check sees a collision.
fn ident(kind: u8, n: u64) -> [u8; 16] {
    let mut id = [kind; 16];
    id[8..].copy_from_slice(&n.to_be_bytes());
    id
}
fn candidates(history: &[(journal::TurnAccepted, journal::TurnTerminal)]) -> Vec<Candidate> {
    history
        .iter()
        .map(|(accepted, terminal)| Candidate {
            operation: accepted.operation,
            generation: accepted.generation,
            prompt: accepted.prompt.clone(),
            answer: terminal.text.clone(),
        })
        .collect()
}

#[test]
fn forty_turns_compose_within_the_window_and_reconstruct_after_replay() {
    let _serial = crate::TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let path = std::path::PathBuf::from(format!(
        "/private/tmp/asura-compose-{:x}",
        u128::from_ne_bytes(asura_platform::random_id())
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .unwrap();
    let scratch = Scratch(path);
    std::fs::create_dir(scratch.0.join("project")).unwrap();
    let runtime = asura_platform::RuntimeDirectory::scratch(&scratch.0, true).unwrap();
    let mut writer = writer::WriterHandle::start(runtime.clone()).unwrap();
    wait(&writer, writer::Command::Initialize { request: [1; 16] });
    let registered = wait(
        &writer,
        writer::Command::Register {
            request: [2; 16],
            location: scratch.0.join("project").to_str().unwrap().into(),
        },
    )
    .replay
    .unwrap();
    let project = *registered.projects.keys().next().unwrap();
    let mut revision = registered.revision;
    drop(registered);
    let conversation = ident(0xC0, 1);
    let budget = Budget {
        window_tokens: 8192,
        output_reservation: 2048,
        overhead_tokens: 0,
        bytes_per_token_milli: 1000,
    };
    let mut accepted_turns = Vec::new();
    for generation in 1..=40u64 {
        let prompt = format!("question {generation}: {}", "detail ".repeat(60));
        let prepared = wait(
            &writer,
            writer::Command::Prepare {
                project,
                conversation: (generation > 1).then_some(conversation),
                expected_generation: generation - 1,
                prompt: prompt.clone(),
            },
        )
        .prepared
        .unwrap();
        // Every completed prior turn arrives, newest first, with no count rule.
        assert_eq!(prepared.history.len() as u64, generation - 1);
        assert!(
            prepared
                .history
                .windows(2)
                .all(|w| w[0].0.generation > w[1].0.generation)
        );
        let composed = compose("rules", &prompt, &candidates(&prepared.history), budget).unwrap();
        assert!(composed.input.len() <= 8192 - 2048);
        let operation = ident(0xA0, generation);
        let turn = journal::TurnAccepted {
            request: ident(0xB0, generation),
            digest: journal::request_digest(journal::Request::Submit {
                project,
                conversation: (generation > 1).then_some(conversation),
                expected_generation: generation - 1,
                prompt: &prompt,
            })
            .unwrap(),
            project,
            original_conversation: (generation > 1).then_some(conversation),
            expected_generation: generation - 1,
            conversation,
            generation,
            task: ident(0xD0, generation),
            operation,
            model: "system".into(),
            configuration_digest: prepared.configuration_digest,
            instructions_digest: Sha256::digest(b"rules").into(),
            input_digest: Sha256::digest(&composed.input).into(),
            prior_operations: composed.shown.clone(),
            prompt: prompt.clone(),
            reserved_output_tokens: 2048,
            event_cursor: 1,
            composition: Some(composed.composition(budget)),
        };
        for record in [
            journal::Record::TurnAccepted(Box::new(turn.clone())),
            journal::Record::StartAuthorized(journal::StartAuthorized {
                operation,
                generation,
                helper: [9; 16],
                input_digest: turn.input_digest,
            }),
            journal::Record::TurnTerminal(journal::TurnTerminal {
                operation,
                generation,
                kind: journal::TerminalKind::Complete,
                cause: journal::Cause::None,
                final_cursor: u64::MAX,
                usage_known: true,
                output_tokens: 12,
                charged_tokens: 12,
                text: format!("answer {generation}: {}", "words ".repeat(60)),
            }),
        ] {
            let admission = matches!(record, journal::Record::TurnAccepted(_)).then_some(
                writer::AdmissionFence {
                    project,
                    configuration_digest: prepared.configuration_digest,
                },
            );
            revision = wait(
                &writer,
                writer::Command::Append {
                    expected_revision: revision,
                    record,
                    admission,
                },
            )
            .replay
            .unwrap()
            .revision;
        }
        accepted_turns.push((turn, composed.input));
    }
    close(&mut writer);
    drop(writer);
    // Replay after restart, then recompose each turn from the journal and the
    // recorded budget inputs (N1): digests and manifest match exactly.
    let mut writer = writer::WriterHandle::start(runtime).unwrap();
    let state = wait(&writer, writer::Command::Open).replay.unwrap();
    assert_eq!(state.conversations[&conversation].generation, 40);
    let late = &accepted_turns[39].0;
    let composition = late.composition.unwrap();
    assert!(composition.omitted_turns > 0, "older turns were omitted");
    assert_eq!(
        late.prior_operations.len() as u32 + composition.omitted_turns,
        39
    );
    drop(state);
    for (turn, input) in &accepted_turns {
        let c = turn.composition.unwrap();
        let prepared = wait(
            &writer,
            writer::Command::Prepare {
                project,
                conversation: Some(conversation),
                expected_generation: 40,
                prompt: "probe".into(),
            },
        )
        .prepared
        .unwrap();
        let prior: Vec<_> = prepared
            .history
            .into_iter()
            .filter(|(accepted, _)| accepted.generation < turn.generation)
            .collect();
        let recomposed = compose(
            "rules",
            &turn.prompt,
            &candidates(&prior),
            Budget {
                window_tokens: c.window_tokens,
                output_reservation: c.output_reservation,
                overhead_tokens: c.overhead_tokens,
                bytes_per_token_milli: c.bytes_per_token_milli,
            },
        )
        .unwrap();
        assert_eq!(&recomposed.input, input, "turn {}", turn.generation);
        assert_eq!(recomposed.manifest_digest, c.manifest_digest);
        assert_eq!(recomposed.shown, turn.prior_operations);
    }
    close(&mut writer);
}
