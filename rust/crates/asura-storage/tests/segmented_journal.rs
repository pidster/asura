//! Segmented journal integration cases through the real writer on real files
//! in private scratch homes (segmented-journal.md, validation). Each test
//! holds the process-wide writer claim in turn.
use asura_platform::journal_gate::{JournalGate, Point};
use asura_platform::{RuntimeDirectory, random_id};
use asura_storage::authority::conversation::*;
use asura_storage::authority::writer::*;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

static WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = PathBuf::from(format!(
            "/private/tmp/asura-segmented-{:x}",
            u128::from_ne_bytes(random_id())
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        // SJ-31: tests never use the account runtime.
        assert!(!path.starts_with(std::env::var("HOME").unwrap_or_default() + "/.asura"));
        Self(path)
    }
    fn runtime(&self) -> RuntimeDirectory {
        RuntimeDirectory::scratch(&self.0, true).unwrap()
    }
    fn control(&self) -> PathBuf {
        self.0.join(".asura/state/control")
    }
    fn project(&self, n: u64) -> String {
        let path = self.0.join(format!("projects/p{n}"));
        fs::create_dir_all(&path).unwrap();
        path.to_str().unwrap().into()
    }
    fn names(&self) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(self.control())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn wait(ticket: &mut Ticket) -> Result<Reply> {
    // Test-harness settling bound only; the writer has no time bound.
    let end = Instant::now() + Duration::from_secs(600);
    loop {
        if let Some(result) = ticket.poll() {
            return result;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn run(writer: &WriterHandle, command: Command) -> Result<Reply> {
    wait(&mut writer.try_submit(command).unwrap())
}
fn close(writer: &mut WriterHandle) {
    writer.close();
    let end = Instant::now() + Duration::from_secs(30);
    while !writer.settled() {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn state(writer: &WriterHandle) -> std::sync::Arc<Replay> {
    run(writer, Command::Open).unwrap().replay.unwrap()
}

/// A writer session with an initialized installation.
struct Session {
    writer: WriterHandle,
    digest: Hash,
}
impl Session {
    fn start(scratch: &Scratch) -> Self {
        let writer = WriterHandle::start(scratch.runtime()).unwrap();
        let opened = run(&writer, Command::Open).unwrap().replay;
        let initialized = opened.as_ref().is_some_and(|s| s.binding.is_some());
        drop(opened);
        if !initialized {
            run(&writer, Command::Initialize { request: [30; 16] }).unwrap();
        }
        let digest = state(&writer).initialization.configuration_digest;
        // The admission fence compares the current configuration digest.
        let digest = match run(
            &writer,
            Command::Prepare {
                project: [0; 16],
                conversation: None,
                expected_generation: 0,
                prompt: "p".into(),
            },
        ) {
            Ok(reply) => reply.prepared.map_or(digest, |p| p.configuration_digest),
            Err(_) => digest,
        };
        Self { writer, digest }
    }
    fn register(&self, scratch: &Scratch, n: u64) -> Id {
        let reply = run(
            &self.writer,
            Command::Register {
                request: random_id(),
                location: scratch.project(n),
            },
        )
        .unwrap();
        reply.project.unwrap().registration.project
    }
    fn configuration(&mut self, project: Id) -> Hash {
        let reply = run(
            &self.writer,
            Command::Prepare {
                project,
                conversation: None,
                expected_generation: 0,
                prompt: "p".into(),
            },
        )
        .unwrap();
        self.digest = reply.prepared.unwrap().configuration_digest;
        self.digest
    }
    fn append(&self, record: Record) -> Result<()> {
        let revision = state(&self.writer).revision;
        run(
            &self.writer,
            Command::Append {
                expected_revision: revision,
                record,
                admission: None,
            },
        )
        .map(|_| ())
    }
    /// Accept and start a turn; returns (conversation, operation, generation).
    fn start_turn(&mut self, project: Id, conversation: Option<Id>) -> (Id, Id, u64) {
        self.start_turn_thinking(project, conversation, 0, 0)
    }
    /// [`Self::start_turn`] with kind 21 thinking level and request codes.
    fn start_turn_thinking(
        &mut self,
        project: Id,
        conversation: Option<Id>,
        thinking_level: u8,
        thinking_request: u8,
    ) -> (Id, Id, u64) {
        let digest = self.configuration(project);
        let current = state(&self.writer);
        let (expected, completed) = match conversation {
            Some(id) => {
                let reply = run(
                    &self.writer,
                    Command::History {
                        project,
                        conversation: Some(id),
                        before_accepted_frame: None,
                        limit: 1,
                    },
                )
                .unwrap();
                let generation = reply.history.unwrap().generation.unwrap();
                (generation, generation)
            }
            None => (0, 0),
        };
        let operation = random_id();
        let target = conversation.unwrap_or_else(random_id);
        let turn = TurnAccepted {
            request: random_id(),
            digest: request_digest(Request::Submit {
                project,
                conversation,
                expected_generation: expected,
                prompt: "prompt",
            })
            .unwrap(),
            project,
            original_conversation: conversation,
            expected_generation: expected,
            conversation: target,
            generation: expected + 1,
            task: random_id(),
            operation,
            model: "system".into(),
            configuration_digest: digest,
            instructions_digest: [1; 32],
            input_digest: [2; 32],
            prior_operations: Vec::new(),
            prompt: "prompt".into(),
            event_cursor: 1,
            composition: Composition {
                omitted_turns: completed as u32,
                ..Composition::default()
            },
            thinking_level,
            thinking_request,
        };
        run(
            &self.writer,
            Command::Append {
                expected_revision: current.revision,
                record: Record::TurnAccepted(Box::new(turn)),
                admission: Some(AdmissionFence {
                    project,
                    configuration_digest: digest,
                }),
            },
        )
        .unwrap();
        drop(current);
        self.append(Record::StartAuthorized(StartAuthorized {
            operation,
            generation: expected + 1,
            helper: random_id(),
            input_digest: [2; 32],
        }))
        .unwrap();
        (target, operation, expected + 1)
    }
    fn finish(&self, operation: Id, generation: u64, offset: u64, tail: &str) {
        self.append(Record::TurnTerminal(TurnTerminal {
            operation,
            generation,
            kind: TerminalKind::Complete,
            cause: Cause::None,
            final_cursor: u64::MAX,
            usage_known: true,
            output_tokens: 1,
            text_offset: offset,
            text: tail.into(),
        }))
        .unwrap();
    }
    fn read(&self, operation: Id, offset: u64) -> (u64, String, u64) {
        let reply = run(
            &self.writer,
            Command::ReadResponse {
                operation,
                offset,
                thinking_offset: 0,
            },
        )
        .unwrap();
        let Some(Record::TurnTerminal(terminal)) = reply.record else {
            panic!("terminal");
        };
        (
            terminal.text_offset,
            terminal.text,
            reply.response_bytes.unwrap(),
        )
    }
    /// The whole response through windows of at most 61,440 bytes.
    fn read_all(&self, operation: Id) -> String {
        let mut text = String::new();
        loop {
            let (start, window, total) = self.read(operation, text.len() as u64);
            assert_eq!(start, text.len() as u64);
            assert!(window.len() <= MAX_TEXT);
            text.push_str(&window);
            if text.len() as u64 == total {
                return text;
            }
            assert!(!window.is_empty());
        }
    }
}
fn segment_files(scratch: &Scratch) -> Vec<(String, Vec<u8>)> {
    scratch
        .names()
        .into_iter()
        .filter(|n| n.starts_with("seg-"))
        .map(|n| {
            let bytes = fs::read(scratch.control().join(&n)).unwrap();
            (n, bytes)
        })
        .collect()
}
fn derived_files(scratch: &Scratch) -> Vec<(String, Vec<u8>)> {
    scratch
        .names()
        .into_iter()
        .filter(|n| n.starts_with("ckpt-") || n.starts_with("run-"))
        .map(|n| {
            let bytes = fs::read(scratch.control().join(&n)).unwrap();
            (n, bytes)
        })
        .collect()
}
fn piece(n: u64) -> String {
    // Multibyte text: windows must cut on character boundaries.
    let unit = format!("{n:08}é·");
    unit.repeat(MAX_TEXT / unit.len())
}

/// SJ-02, SJ-03, SJ-05, SJ-10, SJ-27, SJ-32, SJ-33: a response of about
/// 20 MiB is journaled whole across segment files; files end on frame
/// boundaries; windows, history and prompt reads match before and after a
/// restart and after a rebuild from frames; derived files hold no text.
#[test]
fn sj27_twenty_mib_response_spans_segment_files_and_reads_back_whole() {
    let _guard = WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let mut session = Session::start(&scratch);
    let project = session.register(&scratch, 1);
    let (conversation, operation, generation) = session.start_turn(project, None);
    let mut expected = String::new();
    let mut n = 0;
    while expected.len() < 20 * 1024 * 1024 {
        let text = piece(n);
        session
            .append(Record::ResponseText(ResponseText {
                operation,
                generation,
                offset: expected.len() as u64,
                text: text.clone(),
            }))
            .unwrap();
        expected.push_str(&text);
        n += 1;
    }
    session.finish(operation, generation, expected.len() as u64, "end");
    expected.push_str("end");
    let segments = segment_files(&scratch);
    assert!(segments.len() >= 3, "{:?}", scratch.names());
    for (name, bytes) in &segments {
        assert!(bytes.len() <= 8 * 1024 * 1024, "{name}");
        // SJ-33: frame format 1.
        assert_eq!(&bytes[..10], b"ASURAJ01\0\x01", "{name}");
    }
    assert_eq!(session.read_all(operation), expected);
    // A second turn's history sees the whole first response (Prepare).
    let prepared = run(
        &session.writer,
        Command::Prepare {
            project,
            conversation: Some(conversation),
            expected_generation: generation,
            prompt: "next".into(),
        },
    )
    .unwrap()
    .prepared
    .unwrap();
    // The 20 MiB turn passes the 8 MiB candidate bound: it is omitted, not
    // returned (SJ-D10, SJ-D23).
    assert!(prepared.history.is_empty());
    assert_eq!(prepared.omitted, 1);
    // SJ-32: no response text in checkpoints or runs.
    let marker = piece(7);
    for (name, bytes) in derived_files(&scratch) {
        assert!(
            !bytes.windows(64).any(|w| w == &marker.as_bytes()[..64]),
            "{name}"
        );
    }
    close(&mut session.writer);
    // Restart: one checkpoint and the tail; reads match.
    let session = Session::start(&scratch);
    assert_eq!(session.read_all(operation), expected);
    close(&mut { session }.writer);
    // SJ-05: remove every derived file; the writer rebuilds from frames.
    for (name, _) in derived_files(&scratch) {
        fs::remove_file(scratch.control().join(name)).unwrap();
    }
    let session = Session::start(&scratch);
    assert_eq!(session.read_all(operation), expected);
    assert!(
        !derived_files(&scratch).is_empty(),
        "rebuild wrote checkpoints"
    );
    let mut writer = session.writer;
    close(&mut writer);
}

/// TL-12 and TL-13 (thinking-level.md): about 10 MiB of thinking text in
/// kind 25 segments spans segment files, settles into `H` index pieces and
/// reads back whole in windows that share 61,440 bytes with the response,
/// before and after a restart and after a rebuild from frames. Kind 21
/// returns the level and request; history and derived files hold no
/// thinking text.
#[test]
fn tl12_tl13_thinking_spans_segment_files_and_reads_back_whole() {
    let _guard = WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let mut session = Session::start(&scratch);
    let project = session.register(&scratch, 1);
    let (conversation, operation, generation) = session.start_turn_thinking(project, None, 4, 2);
    let mut thinking = String::new();
    let mut n = 0;
    while thinking.len() < 10 * 1024 * 1024 {
        let text = piece(n);
        session
            .append(Record::ThinkingText(ResponseText {
                operation,
                generation,
                offset: thinking.len() as u64,
                text: text.clone(),
            }))
            .unwrap();
        thinking.push_str(&text);
        n += 1;
    }
    // A cut rewrites the last characters of the journaled thinking.
    let mut cut = thinking.len() - 7;
    while !thinking.is_char_boundary(cut) {
        cut -= 1;
    }
    thinking.truncate(cut);
    thinking.push_str("done.");
    session
        .append(Record::ThinkingText(ResponseText {
            operation,
            generation,
            offset: cut as u64,
            text: "done.".into(),
        }))
        .unwrap();
    session.finish(operation, generation, 0, "answer");
    assert!(segment_files(&scratch).len() >= 2, "{:?}", scratch.names());
    let read_both = |session: &Session| {
        let (mut text, mut held) = (String::new(), String::new());
        loop {
            let reply = run(
                &session.writer,
                Command::ReadResponse {
                    operation,
                    offset: text.len() as u64,
                    thinking_offset: held.len() as u64,
                },
            )
            .unwrap();
            let window = reply.thinking.unwrap();
            assert_eq!((window.level, window.request), (4, 2));
            let Some(Record::TurnTerminal(terminal)) = reply.record else {
                panic!("terminal");
            };
            assert!(window.text.len() + terminal.text.len() <= MAX_TEXT);
            held.truncate(window.offset as usize);
            held.push_str(&window.text);
            text.truncate(terminal.text_offset as usize);
            text.push_str(&terminal.text);
            if held.len() as u64 == window.bytes
                && text.len() as u64 == reply.response_bytes.unwrap()
            {
                return (held, text);
            }
        }
    };
    assert_eq!(read_both(&session), (thinking.clone(), "answer".into()));
    // TL-10: history carries the answer only.
    let prepared = run(
        &session.writer,
        Command::Prepare {
            project,
            conversation: Some(conversation),
            expected_generation: generation,
            prompt: "next".into(),
        },
    )
    .unwrap()
    .prepared
    .unwrap();
    assert_eq!(prepared.history.len(), 1);
    assert_eq!(prepared.history[0].1.text, "answer");
    assert_eq!(prepared.history[0].0.thinking_level, 4);
    let marker = piece(7);
    for (name, bytes) in derived_files(&scratch) {
        assert!(
            !bytes.windows(64).any(|w| w == &marker.as_bytes()[..64]),
            "{name}"
        );
    }
    close(&mut session.writer);
    let session = Session::start(&scratch);
    assert_eq!(read_both(&session), (thinking.clone(), "answer".into()));
    close(&mut { session }.writer);
    for (name, _) in derived_files(&scratch) {
        fs::remove_file(scratch.control().join(name)).unwrap();
    }
    let session = Session::start(&scratch);
    assert_eq!(read_both(&session), (thinking, "answer".into()));
    let mut writer = session.writer;
    close(&mut writer);
}

/// SJ-10, SJ-28: 200 tool-call records of about 64 KiB commit across
/// segment files; their records read back through `C` entries after the
/// operation leaves memory; history pages across runs.
#[test]
fn sj28_two_hundred_large_tool_calls_span_segment_files() {
    let _guard = WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let mut session = Session::start(&scratch);
    let project = session.register(&scratch, 1);
    let (conversation, operation, generation) = session.start_turn(project, None);
    let arguments = format!("{{\"path\":\"{}\"}}", "a".repeat(63_900));
    for ordinal in 1..=MAX_RECORDED_CALLS {
        session
            .append(Record::ToolCall(Box::new(ToolCall {
                operation,
                generation,
                ordinal,
                source: ToolCallSource::GeneratedContent,
                name: "project".into(),
                name_bytes: 7,
                tool_operation: Some(ToolOperation::ReadFile),
                decision: ToolDecision::Denied,
                fault: None,
                busy: None,
                form: ArgumentForm::Verbatim,
                arguments_bytes: arguments.len() as u32,
                arguments: arguments.clone(),
                number_kinds: Vec::new(),
                unnumbered: 0,
                rule_set_revision: 1,
                rule_set_digest: [3; 32],
                command_cut: false,
                whole_command: false,
                refusal: None,
            })))
            .unwrap();
    }
    session.finish(operation, generation, 0, "done");
    assert!(segment_files(&scratch).len() >= 2);
    // More turns move the operation out of memory into runs.
    let mut operations = vec![operation];
    for _ in 0..3 {
        let (_, op, generation) = session.start_turn(project, Some(conversation));
        let text = "x".repeat(MAX_TEXT);
        for k in 0..70u64 {
            session
                .append(Record::ResponseText(ResponseText {
                    operation: op,
                    generation,
                    offset: k * MAX_TEXT as u64,
                    text: text.clone(),
                }))
                .unwrap();
        }
        session.finish(op, generation, 70 * MAX_TEXT as u64, "");
        operations.push(op);
    }
    assert!(!state(&session.writer).operations.contains_key(&operation));
    let reply = run(
        &session.writer,
        Command::ReadResponse {
            operation,
            offset: 0,
            thinking_offset: 0,
        },
    )
    .unwrap();
    let tools = reply.tools.unwrap();
    assert_eq!(tools.len(), MAX_RECORDED_CALLS as usize);
    assert!(tools.iter().all(|t| t.decision == ToolDecision::Denied));
    // History pages newest first across the delta and runs.
    let mut seen = Vec::new();
    let mut before = None;
    loop {
        let page = run(
            &session.writer,
            Command::History {
                project,
                conversation: Some(conversation),
                before_accepted_frame: before,
                limit: 2,
            },
        )
        .unwrap()
        .history
        .unwrap();
        seen.extend(page.entries.iter().map(|e| e.operation));
        if !page.has_more {
            break;
        }
        before = page.entries.last().map(|e| e.accepted_frame);
    }
    operations.reverse();
    assert_eq!(seen, operations);
    close(&mut session.writer);
}

/// SJ-22: a second writer process gets `OwnerBusy` from the `lock` file.
#[test]
fn sj22_second_writer_process_is_owner_busy() {
    if let Ok(home) = std::env::var("ASURA_SJ22_HOME") {
        let runtime = RuntimeDirectory::scratch(Path::new(&home), true).unwrap();
        let result = asura_platform::JournalDirectory::open(
            runtime,
            false,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        assert!(matches!(
            result,
            Err(asura_platform::JournalError::OwnerBusy)
        ));
        return;
    }
    let _guard = WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let mut session = Session::start(&scratch);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "sj22_second_writer_process_is_owner_busy",
            "--nocapture",
        ])
        .env("ASURA_SJ22_HOME", &scratch.0)
        .status()
        .unwrap();
    assert!(status.success(), "the child saw OwnerBusy");
    close(&mut session.writer);
}

/// SJ-23, SJ-25: a roll stalled at its checkpoint sync does not fence;
/// requests are admitted without waiting; the roll completes on release.
/// Shutdown during a stalled roll leaves files from which a restart
/// recovers.
#[test]
fn sj23_sj25_stalled_roll_and_shutdown_during_roll() {
    let _guard = WRITER.lock().unwrap_or_else(|p| p.into_inner());
    for shutdown in [false, true] {
        let scratch = Scratch::new();
        let gate = JournalGate::new(&scratch.0);
        let mut session = Session::start(&scratch);
        let project = session.register(&scratch, 1);
        let (_, operation, generation) = session.start_turn(project, None);
        gate.hold(Point::CheckpointSync);
        let text = "s".repeat(MAX_TEXT);
        let mut offset = 0;
        // Append until a roll reaches the held checkpoint sync.
        let ticket = loop {
            let revision = state(&session.writer).revision;
            let mut ticket = session
                .writer
                .try_submit(Command::Append {
                    expected_revision: revision,
                    record: Record::ResponseText(ResponseText {
                        operation,
                        generation,
                        offset,
                        text: text.clone(),
                    }),
                    admission: None,
                })
                .unwrap();
            let end = Instant::now() + Duration::from_secs(60);
            let done = loop {
                if let Some(result) = ticket.poll() {
                    break Some(result);
                }
                if gate.waiting() > 0 {
                    break None;
                }
                assert!(Instant::now() < end);
                std::thread::sleep(Duration::from_millis(1));
            };
            match done {
                Some(result) => {
                    result.unwrap();
                    offset += text.len() as u64;
                }
                None => break ticket,
            }
        };
        // Held at the checkpoint sync: admission and status never wait.
        let started = Instant::now();
        let queued = session.writer.try_submit(Command::JournalStatus).unwrap();
        let _ = session.writer.status();
        assert!(started.elapsed() < Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(if shutdown { 200 } else { 5000 }));
        assert_eq!(gate.waiting(), 1, "still held; no time bound ends it");
        let mut ticket = ticket;
        let mut queued = queued;
        if shutdown {
            session.writer.close();
            let result = wait(&mut ticket);
            assert!(result.is_err(), "the shutdown cancels the roll");
            drop(gate);
            let end = Instant::now() + Duration::from_secs(30);
            while !session.writer.settled() {
                assert!(Instant::now() < end);
                std::thread::sleep(Duration::from_millis(1));
            }
            drop(queued);
            // Restart recovers: tmp files removed, the turn is interrupted.
            let mut restarted = Session::start(&scratch);
            assert!(scratch.names().iter().all(|n| !n.ends_with(".tmp")));
            let state = state(&restarted.writer);
            assert!(state.active_operation.is_none());
            drop(state);
            close(&mut restarted.writer);
        } else {
            gate.release(Point::CheckpointSync);
            wait(&mut ticket).unwrap();
            wait(&mut queued).unwrap();
            assert!(scratch.names().iter().any(|n| n.starts_with("ckpt-")));
            session
                .append(Record::ResponseText(ResponseText {
                    operation,
                    generation,
                    offset: offset + text.len() as u64,
                    text: text.clone(),
                }))
                .unwrap();
            close(&mut session.writer);
        }
    }
}

/// SJ-34: payload facts of exec_cmd calls are found through `X` entries
/// after their operations leave memory; unknown payloads have none.
#[test]
fn sj34_payload_facts_after_rolls() {
    let _guard = WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let mut session = Session::start(&scratch);
    let project = session.register(&scratch, 1);
    let installation = state(&session.writer).installation_id;
    let mut payloads = Vec::new();
    for _ in 0..4 {
        let (_, operation, generation) = session.start_turn(project, None);
        session
            .append(Record::ToolCall(Box::new(ToolCall {
                operation,
                generation,
                ordinal: 1,
                source: ToolCallSource::GeneratedContent,
                name: "exec_cmd".into(),
                name_bytes: 8,
                tool_operation: Some(ToolOperation::ExecCmd),
                decision: ToolDecision::Run,
                fault: None,
                busy: None,
                form: ArgumentForm::Canonical,
                arguments_bytes: 20,
                arguments: "{\"command\":\"true\"}".into(),
                number_kinds: Vec::new(),
                unnumbered: 0,
                rule_set_revision: 1,
                rule_set_digest: [3; 32],
                command_cut: false,
                whole_command: false,
                refusal: None,
            })))
            .unwrap();
        session
            .append(Record::CaptureSettled(Box::new(CaptureSettled {
                operation,
                generation,
                ordinal: 1,
                state: CaptureState::Complete,
                reason: CaptureReason::None,
                exit_kind: 1,
                exit_value: 0,
                stdout: CaptureStream {
                    eof: true,
                    digest: Some([0xe3; 32]),
                    ..CaptureStream::default()
                },
                stderr: CaptureStream {
                    eof: true,
                    digest: Some([0xe3; 32]),
                    ..CaptureStream::default()
                },
                distinct_numbers: 0,
                numbers: Vec::new(),
                pause_count: 0,
                first_pause_ms: 0,
                paused_ms: 0,
                final_reservation: INITIAL_RESERVATION,
            })))
            .ok();
        session
            .append(Record::ToolResult(ToolResult {
                operation,
                generation,
                ordinal: 1,
                status: 1,
                text: "exit_code=0\nstdout:\n\nstderr:\n".into(),
                next_offset: None,
                truncated: false,
            }))
            .ok();
        // Fill so that rolls move the operation into a run.
        let text = "y".repeat(MAX_TEXT);
        for k in 0..50u64 {
            session
                .append(Record::ResponseText(ResponseText {
                    operation,
                    generation,
                    offset: k * MAX_TEXT as u64,
                    text: text.clone(),
                }))
                .unwrap();
        }
        session.finish(operation, generation, 50 * MAX_TEXT as u64, "");
        payloads.push(asura_storage::payload::payload_id(
            installation,
            exec_cmd_job_id(operation, generation, 1),
            asura_storage::payload::ObjectKind::Stdout,
        ));
    }
    payloads.push(random_id());
    let facts = run(
        &session.writer,
        Command::PayloadFacts {
            payloads: payloads.clone(),
        },
    )
    .unwrap()
    .payload_facts;
    assert_eq!(facts.len(), 4);
    for (fact, payload) in facts.iter().zip(&payloads) {
        assert_eq!(fact.payload, *payload);
        assert!(fact.payloads.contains(payload));
    }
    assert!(matches!(
        session.writer.try_submit(Command::PayloadFacts {
            payloads: vec![[1; 16]; 129]
        }),
        Err(Error::Invalid)
    ));
    close(&mut session.writer);
}

/// SJ-35, SJ-37, SJ-38 (integration): 300 registrations across rolls; every
/// project resolves after a restart; pages in ID order have no gaps or
/// duplicates across a rename and a roll; `only` reads one project; a
/// project outside the recent set restores its latest conversation.
#[test]
fn sj35_sj37_sj38_many_projects_page_and_resolve_across_restart() {
    let _guard = WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let mut session = Session::start(&scratch);
    let first = session.register(&scratch, 0);
    let (old_conversation, operation, generation) = session.start_turn(first, None);
    session.finish(operation, generation, 0, "first");
    let mut projects = vec![first];
    for n in 1..300 {
        projects.push(session.register(&scratch, n));
    }
    // A second registration of the same directory is an alias.
    let alias = run(
        &session.writer,
        Command::Register {
            request: random_id(),
            location: scratch.project(5),
        },
    )
    .unwrap();
    assert_eq!(alias.project.unwrap().registration.project, projects[5]);
    assert!(!state(&session.writer).projects.contains_key(&first));
    close(&mut session.writer);
    let mut session = Session::start(&scratch);
    let page = |session: &Session, after: Option<Id>, before: Option<Id>| {
        run(
            &session.writer,
            Command::Projects {
                after,
                before,
                limit: 8,
            },
        )
        .unwrap()
    };
    let mut listed = Vec::new();
    let mut after = None;
    let mut renamed = false;
    loop {
        let reply = page(&session, after, None);
        listed.extend(reply.projects.iter().map(|p| p.registration.project));
        // A held reply keeps a snapshot; release it before appending.
        let more = reply.more;
        drop(reply);
        if !renamed && listed.len() > 100 {
            // A rename and many appends (a roll) during paging.
            renamed = true;
            let target = projects[200];
            run(
                &session.writer,
                Command::RenameProject {
                    request: random_id(),
                    project: target,
                    expected_name_revision: 0,
                    name: "renamed".into(),
                },
            )
            .unwrap();
            let (_, operation, generation) = session.start_turn(projects[3], None);
            let text = "z".repeat(MAX_TEXT);
            for k in 0..150u64 {
                session
                    .append(Record::ResponseText(ResponseText {
                        operation,
                        generation,
                        offset: k * MAX_TEXT as u64,
                        text: text.clone(),
                    }))
                    .unwrap();
            }
            session.finish(operation, generation, 150 * MAX_TEXT as u64, "");
        }
        if !more {
            break;
        }
        after = listed.last().copied();
    }
    let mut sorted = projects.clone();
    sorted.sort();
    assert_eq!(listed, sorted, "pages have no gaps or duplicates");
    // Descending pages from the end.
    let reply = page(&session, None, Some(sorted[10]));
    let ids: Vec<_> = reply
        .projects
        .iter()
        .map(|p| p.registration.project)
        .collect();
    assert_eq!(ids, sorted[2..10].iter().rev().copied().collect::<Vec<_>>());
    assert!(reply.more);
    // Exact reads.
    let only = run(
        &session.writer,
        Command::Project {
            project: projects[200],
        },
    )
    .unwrap()
    .projects;
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].name, "Renamed");
    assert!(
        run(
            &session.writer,
            Command::Project {
                project: random_id()
            }
        )
        .unwrap()
        .projects
        .is_empty()
    );
    // Restoration of a project outside the recent set (SJ-38).
    let history = run(
        &session.writer,
        Command::History {
            project: first,
            conversation: None,
            before_accepted_frame: None,
            limit: 8,
        },
    )
    .unwrap()
    .history
    .unwrap();
    assert_eq!(history.conversation, Some(old_conversation));
    assert_eq!(history.entries.len(), 1);
    close(&mut session.writer);
}

/// SJ-29 (bounded here): a longer journal opens with one checkpoint and the
/// tail; startup time is recorded. `ASURA_SJ29_MIB` raises the size.
#[test]
fn sj29_long_journal_startup_is_recorded() {
    let _guard = WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let target: usize = std::env::var("ASURA_SJ29_MIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    let scratch = Scratch::new();
    let mut session = Session::start(&scratch);
    let project = session.register(&scratch, 1);
    let text = "w".repeat(MAX_TEXT);
    let mut written = 0usize;
    let mut operations = Vec::new();
    while written < target * 1024 * 1024 {
        let (_, operation, generation) = session.start_turn(project, None);
        for k in 0..64u64 {
            session
                .append(Record::ResponseText(ResponseText {
                    operation,
                    generation,
                    offset: k * MAX_TEXT as u64,
                    text: text.clone(),
                }))
                .unwrap();
        }
        session.finish(operation, generation, 64 * MAX_TEXT as u64, "");
        written += 64 * MAX_TEXT;
        operations.push(operation);
    }
    close(&mut session.writer);
    let segments = segment_files(&scratch).len();
    let started = Instant::now();
    let session = Session::start(&scratch);
    let elapsed = started.elapsed();
    println!("SJ-29 startup: {target} MiB, {segments} segment files, {elapsed:?}");
    let status = run(&session.writer, Command::JournalStatus)
        .unwrap()
        .status
        .unwrap();
    println!("SJ-29 status: {status:?}");
    // Lookups through the index are correct.
    for operation in [operations[0], operations[operations.len() / 2]] {
        let (_, _, total) = session.read(operation, 0);
        assert_eq!(total, 64 * MAX_TEXT as u64);
    }
    let mut writer = session.writer;
    close(&mut writer);
}

/// A small APFS disk image in a private scratch directory.
struct DiskImage {
    image: PathBuf,
    mount: PathBuf,
}
impl DiskImage {
    fn new(megabytes: u64) -> Self {
        let base = PathBuf::from(format!(
            "/private/tmp/asura-sj17-{:x}",
            u128::from_ne_bytes(random_id())
        ));
        fs::DirBuilder::new().mode(0o700).create(&base).unwrap();
        let image = base.join("disk.dmg");
        let mount = base.join("mount");
        fs::create_dir(&mount).unwrap();
        let created = std::process::Command::new("/usr/bin/hdiutil")
            .args(["create", "-quiet", "-size"])
            .arg(format!("{megabytes}m"))
            .args(["-fs", "APFS", "-volname", "asura-sj17"])
            .arg(&image)
            .status()
            .unwrap();
        assert!(created.success(), "hdiutil create");
        let attached = std::process::Command::new("/usr/bin/hdiutil")
            .args(["attach", "-quiet", "-nobrowse", "-noverify", "-mountpoint"])
            .arg(&mount)
            .arg(&image)
            .status()
            .unwrap();
        assert!(attached.success(), "hdiutil attach");
        Self { image, mount }
    }
}
impl Drop for DiskImage {
    fn drop(&mut self) {
        let _ = std::process::Command::new("/usr/bin/hdiutil")
            .args(["detach", "-quiet", "-force"])
            .arg(&self.mount)
            .status();
        if let Some(base) = self.image.parent() {
            let _ = fs::remove_dir_all(base);
        }
    }
}
fn free_bytes(runtime: &RuntimeDirectory) -> u64 {
    asura_platform::JournalDirectory::open_read_only(
        runtime.clone(),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap()
    .free_bytes()
    .unwrap()
}
fn fill(path: &Path, bytes: u64) {
    let mut file = fs::File::create(path).unwrap();
    let block = vec![0u8; 1 << 20];
    let mut remaining = bytes;
    while remaining > 0 {
        let n = remaining.min(block.len() as u64) as usize;
        if std::io::Write::write_all(&mut file, &block[..n]).is_err() {
            break;
        }
        remaining -= n as u64;
    }
    let _ = file.sync_all();
}

/// SJ-17 (integration, constrained APFS disk image): free space below what a
/// roll needs holds normal records with `journal_disk_space` and writes
/// nothing; freeing space outside Asura ends the hold at the next recheck;
/// with the disk full, an append inside the preallocated active segment
/// either commits (preallocation effect) or is held with the file size
/// unchanged, and the journal is never fenced.
#[test]
fn sj17_disk_floor_and_full_disk_hold_without_fencing() {
    let _guard = WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let disk = DiskImage::new(160);
    let home = disk.mount.join("home");
    fs::DirBuilder::new().mode(0o700).create(&home).unwrap();
    let scratch = Scratch(home);
    let runtime = scratch.runtime();
    let mut session = Session::start(&scratch);
    let project = session.register(&scratch, 1);
    let (_, operation, generation) = session.start_turn(project, None);
    // Leave 56 MiB free: appends inside the active segment fit, a roll
    // (32 MiB) beside the roll reserve does not.
    let filler = disk.mount.join("filler");
    let free = free_bytes(&runtime);
    fill(&filler, free.saturating_sub(56 << 20));
    let text = "d".repeat(MAX_TEXT);
    let mut offset = 0u64;
    let held = loop {
        match session.append(Record::ResponseText(ResponseText {
            operation,
            generation,
            offset,
            text: text.clone(),
        })) {
            Ok(()) => offset += text.len() as u64,
            Err(error) => break error,
        }
        assert!(offset < 64 << 20, "the floor must hold before 64 MiB");
    };
    assert_eq!(held, Error::JournalDiskSpace);
    assert_eq!(session.writer.status().held, Some(HoldReason::Disk));
    let before: Vec<_> = segment_files(&scratch);
    // Freeing space outside Asura: the next recheck ends the hold.
    fs::remove_file(&filler).unwrap();
    run(&session.writer, Command::JournalStatus).unwrap();
    assert_eq!(session.writer.status().held, None);
    session
        .append(Record::ResponseText(ResponseText {
            operation,
            generation,
            offset,
            text: text.clone(),
        }))
        .unwrap();
    offset += text.len() as u64;
    assert!(segment_files(&scratch).len() > before.len(), "the roll ran");
    // Disk full: no free byte outside the preallocated segment.
    fill(&filler, free_bytes(&runtime));
    let result = session.append(Record::ResponseText(ResponseText {
        operation,
        generation,
        offset,
        text: text.clone(),
    }));
    println!("SJ-17 full-disk append inside the active segment: {result:?}");
    match result {
        Ok(()) => offset += text.len() as u64,
        Err(error) => assert!(
            matches!(error, Error::JournalDiskSpace | Error::JournalQuota),
            "{error:?}"
        ),
    }
    // Never fenced: closure records still commit once space returns.
    fs::remove_file(&filler).unwrap();
    session
        .append(Record::CancelRequested(CancelRequested {
            operation,
            generation,
            cause: Cause::UserCancel,
        }))
        .unwrap();
    session
        .append(Record::TurnTerminal(TurnTerminal {
            operation,
            generation,
            kind: TerminalKind::Cancelled,
            cause: Cause::UserCancel,
            final_cursor: u64::MAX,
            usage_known: false,
            output_tokens: 0,
            text_offset: offset,
            text: String::new(),
        }))
        .unwrap();
    close(&mut session.writer);
    drop(scratch);
}

/// A closed multi-segment journal: (scratch, project, operations).
fn closed_journal(turns: usize) -> (Scratch, Id, Vec<Id>) {
    let scratch = Scratch::new();
    let mut session = Session::start(&scratch);
    let project = session.register(&scratch, 1);
    let text = "c".repeat(MAX_TEXT);
    let mut operations = Vec::new();
    for _ in 0..turns {
        let (_, operation, generation) = session.start_turn(project, None);
        for k in 0..60u64 {
            session
                .append(Record::ResponseText(ResponseText {
                    operation,
                    generation,
                    offset: k * MAX_TEXT as u64,
                    text: text.clone(),
                }))
                .unwrap();
        }
        session.finish(operation, generation, 60 * MAX_TEXT as u64, "");
        operations.push(operation);
    }
    close(&mut session.writer);
    (scratch, project, operations)
}
fn snapshot(scratch: &Scratch) -> Vec<(String, Vec<u8>)> {
    scratch
        .names()
        .into_iter()
        .map(|n| {
            let bytes = fs::read(scratch.control().join(&n)).unwrap();
            (n, bytes)
        })
        .collect()
}
/// The read-only inspection scan of a home (the service's startup scan).
fn inspect(scratch: &Scratch) -> std::result::Result<(), String> {
    use asura_storage::authority::journal::{Journal, Opened};
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let scan = scratch
        .runtime()
        .authority_source(&cancel)
        .map_err(|e| format!("{e:?}"))?;
    let asura_platform::AuthoritySource::Journal(reader) = scan.source else {
        return Err("no journal".into());
    };
    let directory = reader
        .directory(cancel.clone())
        .map_err(|e| format!("{e:?}"))?;
    match Journal::open(directory, false, &cancel) {
        Ok(Opened::Journal(_)) => Ok(()),
        Ok(Opened::Empty(_)) => Err("empty".into()),
        Err(failure) => Err(format!("{failure:?}")),
    }
}

/// SJ-18, SJ-19, SJ-20, SJ-21, SJ-26 (real files): a torn newest segment, a
/// damaged sealed frame, a missing interior segment and a damaged run page
/// each fail with repair required; damaged reads fence the writer; neither
/// the writer nor the read-only scan changes any file.
#[test]
fn sj18_to_sj21_sj26_damage_on_real_files_is_detected_and_preserved() {
    let _guard = WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let (scratch, project, operations) = closed_journal(6);
    let segments = segment_files(&scratch);
    assert!(segments.len() >= 3, "{:?}", scratch.names());
    inspect(&scratch).unwrap();
    let pristine = snapshot(&scratch);
    let control = scratch.control();
    let restore = |snapshot: &[(String, Vec<u8>)]| {
        for name in scratch.names() {
            fs::remove_file(control.join(name)).unwrap();
        }
        for (name, bytes) in snapshot {
            let path = control.join(name);
            fs::write(&path, bytes).unwrap();
            fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
                .unwrap();
        }
    };
    // SJ-18: torn newest segment.
    let (newest, bytes) = segments.last().unwrap().clone();
    assert!(bytes.len() > 1000, "the newest segment holds frames");
    fs::write(control.join(&newest), &bytes[..bytes.len() - 37]).unwrap();
    let torn = snapshot(&scratch);
    assert_eq!(inspect(&scratch).unwrap_err(), "Replay(IncompleteTail)");
    let writer = WriterHandle::start(scratch.runtime()).unwrap();
    assert!(matches!(
        run(&writer, Command::Open),
        Err(Error::RepairRequired)
    ));
    let mut writer = writer;
    close(&mut writer);
    assert_eq!(snapshot(&scratch), torn, "SJ-26: nothing changed");
    restore(&pristine);
    // SJ-20: a missing interior segment.
    let (interior, _) = segments[1].clone();
    fs::remove_file(control.join(&interior)).unwrap();
    let missing = snapshot(&scratch);
    assert!(inspect(&scratch).is_err());
    let writer = WriterHandle::start(scratch.runtime()).unwrap();
    assert!(matches!(
        run(&writer, Command::Open),
        Err(Error::RepairRequired)
    ));
    let mut writer = writer;
    close(&mut writer);
    assert_eq!(snapshot(&scratch), missing);
    restore(&pristine);
    // SJ-19: a damaged frame in a sealed segment fails the read and fences.
    let (sealed, mut bytes) = segments[0].clone();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 1;
    fs::write(control.join(&sealed), &bytes).unwrap();
    let damaged = snapshot(&scratch);
    let writer = WriterHandle::start(scratch.runtime()).unwrap();
    run(&writer, Command::Open).unwrap();
    let mut failed = false;
    for operation in &operations {
        if let Err(error) = run(
            &writer,
            Command::ReadResponse {
                operation: *operation,
                offset: 0,
                thinking_offset: 0,
            },
        ) {
            assert_eq!(error, Error::RepairRequired);
            failed = true;
            break;
        }
        let _ = (0..60u64).try_for_each(|k| {
            run(
                &writer,
                Command::ReadResponse {
                    operation: *operation,
                    offset: k * MAX_TEXT as u64,
                    thinking_offset: 0,
                },
            )
            .map(|_| ())
        });
    }
    assert!(failed, "a read reached the damaged frame");
    assert!(matches!(
        run(
            &writer,
            Command::Projects {
                after: None,
                before: None,
                limit: 1
            }
        ),
        Err(Error::RepairRequired)
    ));
    let mut writer = writer;
    close(&mut writer);
    // Evidence preserved: the damaged file is unchanged and no segment file
    // was removed (the writer's open appended its owner generation before
    // the read found the damage).
    let after = snapshot(&scratch);
    let segment_after = |name: &str| {
        after
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, b)| b.clone())
    };
    assert_eq!(segment_after(&sealed), Some(bytes.clone()));
    for (name, _) in damaged.iter().filter(|(n, _)| n.starts_with("seg-")) {
        assert!(segment_after(name).is_some(), "{name} kept");
    }
    restore(&pristine);
    // SJ-21: a damaged run page fails the lookup that reads it and fences.
    let run_name = scratch
        .names()
        .into_iter()
        .find(|n| n.starts_with("run-"))
        .unwrap();
    let path = control.join(&run_name);
    let mut bytes = fs::read(&path).unwrap();
    for at in (100..bytes.len().saturating_sub(100)).step_by(512) {
        bytes[at] ^= 1;
    }
    fs::write(&path, &bytes).unwrap();
    let writer = WriterHandle::start(scratch.runtime()).unwrap();
    // Selection fails on the damaged run header or later on a page.
    let opened = run(&writer, Command::Open);
    if opened.is_ok() {
        let result = run(
            &writer,
            Command::ReadResponse {
                operation: operations[0],
                offset: 0,
                thinking_offset: 0,
            },
        );
        assert!(matches!(result, Err(Error::RepairRequired)), "{result:?}");
    }
    let mut writer = writer;
    close(&mut writer);
    let _ = project;
}

/// SJ-11, SJ-24 (real files): with eight level-0 runs, merge steps run
/// between commands; appends and reads continue; the merged level-1 run is
/// listed after the next rolls and the inputs are retired.
#[test]
fn sj11_sj24_merges_interleave_with_commands_on_real_files() {
    let _guard = WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let mut session = Session::start(&scratch);
    let project = session.register(&scratch, 1);
    let text = "m".repeat(MAX_TEXT);
    let runs = |scratch: &Scratch| -> Vec<(String, u8)> {
        scratch
            .names()
            .into_iter()
            .filter(|n| n.starts_with("run-") && n.ends_with(".idx"))
            .map(|n| {
                let bytes = fs::read(scratch.control().join(&n)).unwrap();
                (n, bytes[10])
            })
            .collect()
    };
    let mut operations = Vec::new();
    let mut turn = |session: &mut Session| {
        let (_, operation, generation) = session.start_turn(project, None);
        for k in 0..70u64 {
            session
                .append(Record::ResponseText(ResponseText {
                    operation,
                    generation,
                    offset: k * MAX_TEXT as u64,
                    text: text.clone(),
                }))
                .unwrap();
        }
        session.finish(operation, generation, 70 * MAX_TEXT as u64, "");
        operations.push(operation);
    };
    while runs(&scratch).len() < 8 {
        turn(&mut session);
    }
    // Commands keep running while merge steps wait between them.
    for _ in 0..3 {
        turn(&mut session);
    }
    assert!(
        runs(&scratch).iter().any(|(_, level)| *level == 1),
        "a level-1 run is listed: {:?}",
        runs(&scratch)
    );
    for operation in &operations {
        let reply = run(
            &session.writer,
            Command::ReadResponse {
                operation: *operation,
                offset: 0,
                thinking_offset: 0,
            },
        )
        .unwrap();
        assert_eq!(reply.response_bytes, Some(70 * MAX_TEXT as u64));
    }
    let status = run(&session.writer, Command::JournalStatus)
        .unwrap()
        .status
        .unwrap();
    assert!(status.merges >= 1, "{status:?}");
    assert!(status.runs < 11, "{status:?}");
    close(&mut session.writer);
    inspect(&scratch).unwrap();
}

/// A job-mode call in a started turn: kinds 14, 26 and 15. Returns the job.
fn j2_job_call(session: &Session, operation: Id, generation: u64, ordinal: u32) -> Id {
    session
        .append(Record::ToolCall(Box::new(ToolCall {
            operation,
            generation,
            ordinal,
            source: ToolCallSource::GeneratedContent,
            name: "exec_cmd".into(),
            name_bytes: 8,
            tool_operation: Some(ToolOperation::ExecCmd),
            decision: ToolDecision::Run,
            fault: None,
            busy: None,
            form: ArgumentForm::Canonical,
            arguments_bytes: 20,
            arguments: "{\"command\":\"true\"}".into(),
            number_kinds: Vec::new(),
            unnumbered: 0,
            rule_set_revision: 1,
            rule_set_digest: [3; 32],
            command_cut: false,
            whole_command: false,
            refusal: None,
        })))
        .unwrap();
    let origin = JobOrigin::Model {
        operation,
        generation,
        ordinal,
    };
    session
        .append(Record::JobIntent(JobIntent {
            origin,
            agent: 0,
            time: 1_760_000_000_000,
        }))
        .unwrap();
    session
        .append(Record::ToolResult(ToolResult {
            operation,
            generation,
            ordinal,
            status: 1,
            text: "job".into(),
            next_offset: None,
            truncated: false,
        }))
        .unwrap();
    job_id(&origin)
}
fn j2_exited(job: Id) -> Record {
    let stream = JobStream {
        capture: CaptureStream {
            eof: true,
            digest: Some([0xe3; 32]),
            ..CaptureStream::default()
        },
        lines: 0,
    };
    Record::JobSettled(Box::new(JobSettled {
        job,
        outcome: JobOutcome::Exited,
        reason: JobReason::None,
        exit_kind: 1,
        exit_value: 0,
        cleanup: JobCleanup::Confirmed,
        unconfirmed_reason: 0,
        escapees: 0,
        elapsed_ms: 1,
        capture_state: JobCaptureState::Complete,
        capture_reason: CaptureReason::None,
        stdout: stream.clone(),
        stderr: stream,
        distinct_numbers: 0,
        numbers: Vec::new(),
        pause_count: 0,
        first_pause_ms: 0,
        paused_ms: 0,
        final_reservation: INITIAL_RESERVATION,
        settled_time: 1_760_000_000_002,
    }))
}
fn j2_facts(session: &Session, payloads: Vec<Id>) -> Vec<PayloadFact> {
    run(&session.writer, Command::PayloadFacts { payloads })
        .unwrap()
        .payload_facts
}

/// J2T-03 (integration): rolls with live jobs on real files. Job A settles
/// before the rolls (its `X` form 2 entries reach an index run), job B stays
/// unsettled across the rolls (the checkpoint holds it), job C settles after
/// the last roll (the delta answers). `PayloadFacts` answers each, before
/// and after a restart from the checkpoint; B's `interrupted` kind 28 then
/// commits and its facts carry no settlement.
#[test]
fn j2t_03_rolls_with_live_jobs_restart_and_payload_facts() {
    let _guard = WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let mut session = Session::start(&scratch);
    let project = session.register(&scratch, 1);
    let installation = state(&session.writer).installation_id;
    let (conversation, operation, generation) = session.start_turn(project, None);
    let a = j2_job_call(&session, operation, generation, 1);
    session.append(j2_exited(a)).unwrap();
    let b = j2_job_call(&session, operation, generation, 2);
    // Fill so that rolls move A's entries into a run and checkpoint B.
    let text = "y".repeat(MAX_TEXT);
    for k in 0..160u64 {
        session
            .append(Record::ResponseText(ResponseText {
                operation,
                generation,
                offset: k * MAX_TEXT as u64,
                text: text.clone(),
            }))
            .unwrap();
    }
    session.finish(operation, generation, 160 * MAX_TEXT as u64, "");
    let (_, second, second_generation) = session.start_turn(project, None);
    let c = j2_job_call(&session, second, second_generation, 1);
    session.append(j2_exited(c)).unwrap();
    let current = state(&session.writer);
    assert!(current.base > 0, "a checkpoint exists");
    let b_job = current.jobs[&b].clone();
    assert!(
        b_job.intent.sequence < current.base,
        "B is in the checkpoint"
    );
    assert!(current.sequence > current.base, "C settled in the delta");
    let base = current.base;
    drop(current);
    let [a_out, a_err] = job_payload_ids(installation, a);
    let [b_out, _] = job_payload_ids(installation, b);
    let [_, c_err] = job_payload_ids(installation, c);
    let check = |session: &Session, b_active: bool| {
        let facts = j2_facts(session, vec![a_out, a_err, b_out, c_err, random_id()]);
        assert_eq!(facts.len(), 4);
        for (fact, (payload, job, op)) in facts.iter().zip([
            (a_out, a, operation),
            (a_err, a, operation),
            (b_out, b, operation),
            (c_err, c, second),
        ]) {
            assert_eq!(
                (fact.payload, fact.call, fact.operation),
                (payload, job, op)
            );
            assert_eq!(fact.payloads, job_payload_ids(installation, job));
            assert!(fact.payloads.contains(&payload));
        }
        for fact in [&facts[0], &facts[3]] {
            let (state, sequence, streams) = fact.settled.unwrap();
            assert_eq!(state, CaptureState::Complete);
            assert!(sequence > fact.intent_sequence);
            assert_eq!(streams, [(0, Some([0xe3; 32])); 2]);
            assert!(!fact.active);
        }
        assert_eq!(facts[2].intent_sequence, b_job.intent.sequence);
        assert_eq!(facts[2].settled, None);
        assert_eq!(facts[2].active, b_active);
    };
    check(&session, true);
    // A's kind 28 is before the checkpoint (its `X` is in a run); C's after.
    let facts = j2_facts(&session, vec![a_out, c_err]);
    assert!(facts[0].settled.unwrap().1 < base);
    assert!(facts[1].settled.unwrap().1 > base);
    // Restart from the checkpoint and tail: B is still unsettled.
    close(&mut session.writer);
    let session = Session::start(&scratch);
    let reopened = state(&session.writer);
    let jobs = reopened.unsettled_jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!((jobs[0].job, jobs[0].intent), (b, b_job.intent));
    assert_eq!(
        (jobs[0].conversation, jobs[0].project),
        (conversation, project)
    );
    assert!(reopened.owner_generation > jobs[0].owner_generation);
    drop(reopened);
    check(&session, true);
    // The start-time recovery settlement (J2-E5).
    session
        .append(Record::JobSettled(Box::new(JobSettled::interrupted(
            b,
            1_760_000_000_001,
        ))))
        .unwrap();
    assert!(state(&session.writer).jobs.is_empty());
    check(&session, false);
    let mut session = session;
    close(&mut session.writer);
}
