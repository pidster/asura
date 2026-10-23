//! EPG-T unit tests (execution-path-guard.md, TUI and owner hold EPG-R5):
//! each warning text, the held entry's line and keys, the Hold-then-Drop
//! path, `/paths` parsing, replies and error texts, and the restored log's
//! warnings. Fixtures only; no service, thread or timer.
use super::*;
use crate::tui::jobs::FoldUpdate;
use crate::tui::queue::{Request as QueueRequest, Update as QueueUpdate};
use crossterm::event::{Event, KeyEvent};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

const PROJECT: [u8; 16] = [1; 16];
const CONVERSATION: [u8; 16] = [2; 16];

fn app() -> App {
    let mut app = App::new();
    app.view.connection = "connected";
    app.view.installation = "graph_ready";
    app.view.epoch = Some("epoch".into());
    app.projects = Some(vec![pb::ProjectReply {
        project_id: Some(PROJECT.to_vec()),
        location: Some("/projects/p".into()),
        name: Some("Fixture".into()),
        current: Some(true),
        ..Default::default()
    }]);
    app.selected_project = Some(PROJECT);
    app.restoration_ready = true;
    app.conversation_id = Some(CONVERSATION.to_vec());
    app.conversation_generation = 1;
    app.person.supported = Some(true);
    app
}
fn press(app: &mut App, code: KeyCode) {
    app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::empty())));
}
fn render(app: &mut App) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    terminal.backend().buffer().clone()
}
fn screen(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}
fn warning(k: u32, path: &str) -> pb::PathWarning {
    pb::PathWarning {
        kind: Some(k),
        path: Some(path.into()),
    }
}
fn texts(entry: &LogEntry) -> Vec<String> {
    super::super::person::entry_rows(entry, Style::default())
        .into_iter()
        .map(|(t, _)| t)
        .collect()
}
/// A kind 3 queue entry of the selected conversation.
fn queued(input: u8, outside: bool, state: u32) -> pb::ConversationQueueEntry {
    pb::ConversationQueueEntry {
        input_id: Some(vec![input; 16]),
        project_id: Some(PROJECT.to_vec()),
        conversation_id: Some(CONVERSATION.to_vec()),
        kind: Some(asura_control::person::ENTRY_KIND),
        state: Some(state),
        text: Some("make".into()),
        sequence: Some(u64::from(input)),
        command_cwd: Some(".".into()),
        command_outside_sandbox: outside.then_some(true),
        ..Default::default()
    }
}
fn held(input: u8, warnings: Vec<pb::PathWarning>) -> pb::ConversationQueueEntry {
    pb::ConversationQueueEntry {
        hold_reason: Some(PATHS_CHANGED.into()),
        path_warnings: warnings,
        ..queued(input, true, 2)
    }
}
fn checking(input: u8) -> pb::ConversationQueueEntry {
    pb::ConversationQueueEntry {
        waiting_reason: Some(pb::QueueWaitReason::PathCheck as i32),
        waiting_text: Some("waiting: path check".into()),
        ..queued(input, false, 1)
    }
}
/// The app with the queue showing `entries`, drawn, in log focus.
fn in_log(entries: Vec<pb::ConversationQueueEntry>) -> App {
    let mut app = app();
    app.queue_entries = entries;
    app.person_queue_seen();
    render(&mut app);
    app.history_focus = true;
    app.history_scroll = app.history_scroll_max;
    app
}
fn take_decision(app: &mut App) -> pb::ConversationQueueDecision {
    match app.queue_request.take() {
        Some(QueueRequest::Decision(d)) => d,
        other => panic!("decision expected, got {other:?}"),
    }
}
fn reply_to(app: &mut App, decision: pb::ConversationQueueDecision, outcome: Result<(), &str>) {
    let request_id = decision.request_id.clone();
    app.queue_finished(QueueUpdate {
        request: QueueRequest::Decision(decision),
        outcome: outcome
            .map(|()| pb::ConversationQueueReply {
                request_id,
                full_text: Some(false),
                revision: Some(2),
                ..Default::default()
            })
            .map_err(str::to_owned),
        done: true,
    });
}

// ----- Warning texts -----

#[test]
fn each_warning_kind_has_its_line_and_more_is_counted() {
    let warnings = vec![
        warning(kind::MODEL_CHANGED, "build.rs"),
        warning(kind::NOT_APPROVED, ".git/hooks/pre-commit"),
        warning(kind::MODEL_CHANGED, "package.json"),
        warning(kind::CHANGED_SINCE_APPROVAL, ".envrc"),
        warning(kind::TOO_LARGE, ".git/config"),
        warning(kind::OVERFLOW, "."),
    ];
    assert_eq!(
        warning_lines(&warnings, 3),
        [
            "⚠ changed by the model since your last command: build.rs, package.json",
            "⚠ not approved: .git/hooks/pre-commit · /paths approve",
            "⚠ changed since you approved it: .envrc",
            "⚠ too large to approve (over 1 MiB): .git/config",
            "⚠ more paths than Asura checks: changes beyond the bound are not detected",
            "⚠ and 3 more",
        ]
    );
    assert!(warning_lines(&[], 0).is_empty());
    assert_eq!(warning_lines(&[], 17), ["⚠ and 17 more"]);
    // Paths are shown literally: a control character is a space.
    assert_eq!(
        warning_lines(&[warning(kind::MODEL_CHANGED, "a\u{1b}[2Jb.rs")], 0),
        ["⚠ changed by the model since your last command: a [2Jb.rs"]
    );
}

#[test]
fn warnings_sit_between_the_header_and_the_output_in_alert_colour() {
    let entry = LogEntry {
        input: Some([3; 16]),
        command: Some("cargo build".into()),
        path_warnings: vec![warning(kind::MODEL_CHANGED, "build.rs")],
        fold_done: true,
        pages: [
            Some(pb::JobStreamPage {
                stream: Some(pb::JobStream::Stdout as i32),
                text: Some("Compiling\n".into()),
                start: Some(0),
                ..Default::default()
            }),
            None,
        ],
        ..Default::default()
    };
    let rows = super::super::person::entry_rows(&entry, Style::default());
    let at = |text: &str| rows.iter().position(|(t, _)| t == text).unwrap();
    let line = at("⚠ changed by the model since your last command: build.rs");
    assert_eq!(at("! cargo build"), 0);
    assert!(line < at("Compiling"));
    assert_eq!(rows[line].1.fg, Some(super::super::person::ALERT));
    // A `!!` entry: the PCR22 warning first, then the path lines.
    let outside = LogEntry {
        outside: true,
        ..entry
    };
    let lines = texts(&outside);
    assert_eq!(lines[1], super::super::person::OUTSIDE_WARNING);
    assert_eq!(
        lines[2],
        "⚠ changed by the model since your last command: build.rs"
    );
}

// ----- The held `!!` entry -----

#[test]
fn a_held_entry_shows_its_warnings_and_the_held_line() {
    let app = in_log(vec![held(
        4,
        vec![warning(kind::MODEL_CHANGED, "Makefile")],
    )]);
    let entry = &app.person.log[0];
    assert!(entry.paths_held && entry.outside);
    let lines = texts(entry);
    assert_eq!(
        &lines[..4],
        [
            "!! make",
            super::super::person::OUTSIDE_WARNING,
            "⚠ changed by the model since your last command: Makefile",
            HELD_TEXT,
        ]
    );
    // While held, the fold's "waiting for its turn" note is not shown.
    let mut waiting = entry.clone();
    waiting.fold_note = Some("waiting for its turn in the queue".into());
    assert!(
        !texts(&waiting)
            .iter()
            .any(|t| t.contains("waiting for its turn"))
    );
    assert_eq!(
        app.person_log_hint(),
        Some("↵ run the held command · Esc drop it")
    );
    assert!(
        app.hints()
            .starts_with("↵ run the held command · Esc drop it · ↑↓ scroll history"),
        "{}",
        app.hints()
    );
}

#[test]
fn enter_on_the_held_entry_resumes_it_and_keeps_the_draft() {
    let mut app = in_log(vec![held(
        4,
        vec![warning(kind::MODEL_CHANGED, "Makefile")],
    )]);
    app.insert("draft text");
    press(&mut app, KeyCode::Enter);
    let decision = take_decision(&mut app);
    assert_eq!(decision.action, Some(1), "Resume");
    assert_eq!(decision.input_id.as_deref(), Some(&[4; 16][..]));
    assert!(app.history_focus);
    assert_eq!(app.notice, "Confirming the held command");
    reply_to(&mut app, decision, Ok(()));
    assert!(!app.person.log[0].paths_held);
    assert_eq!(
        app.notice,
        "Command confirmed; Asura checks its paths again, then runs it"
    );
    assert_eq!(app.draft(), "draft text", "a key decision keeps the draft");
    // The queue shows it running: the held line is gone, the warning stays.
    app.queue_entries = vec![pb::ConversationQueueEntry {
        hold_reason: None,
        path_warnings: vec![warning(kind::MODEL_CHANGED, "Makefile")],
        ..queued(4, true, 3)
    }];
    app.person_queue_seen();
    let lines = texts(&app.person.log[0]);
    assert!(!lines.contains(&HELD_TEXT.to_owned()));
    assert!(lines.contains(&"⚠ changed by the model since your last command: Makefile".to_owned()));
}

#[test]
fn a_resume_that_finds_a_newer_change_holds_again_with_the_new_list() {
    let mut app = in_log(vec![held(
        4,
        vec![warning(kind::MODEL_CHANGED, "Makefile")],
    )]);
    app.queue_entries = vec![held(
        4,
        vec![
            warning(kind::MODEL_CHANGED, "Makefile"),
            warning(kind::MODEL_CHANGED, "build.rs"),
        ],
    )];
    app.person_queue_seen();
    let entry = &app.person.log[0];
    assert!(entry.paths_held);
    assert!(texts(entry).contains(
        &"⚠ changed by the model since your last command: Makefile, build.rs".to_owned()
    ));
}

#[test]
fn esc_on_the_held_entry_drops_it_and_nothing_runs() {
    let mut app = in_log(vec![held(
        4,
        vec![warning(kind::MODEL_CHANGED, "Makefile")],
    )]);
    press(&mut app, KeyCode::Esc);
    assert!(app.history_focus, "Esc acted on the entry, not the focus");
    let decision = take_decision(&mut app);
    assert_eq!(decision.action, Some(2), "Drop");
    reply_to(&mut app, decision, Ok(()));
    let entry = &app.person.log[0];
    assert!(entry.dropped && !entry.paths_held && entry.fold_done);
    assert_eq!(app.notice, "Command dropped; it did not run");
    let lines = texts(entry);
    assert_eq!(lines.last().map(String::as_str), Some(DROPPED_TEXT));
    assert!(!lines.contains(&HELD_TEXT.to_owned()));
    // A dropped entry asks for no fold.
    assert!(app.person_folds().is_empty());
    // Esc again: nothing to drop; Esc leaves the log.
    render(&mut app);
    press(&mut app, KeyCode::Esc);
    assert!(!app.history_focus);
    assert!(app.queue_request.is_none());
}

#[test]
fn esc_on_a_checking_entry_holds_then_drops_it() {
    let mut app = in_log(vec![checking(5)]);
    // A checking entry the person staged here has a log entry; the queue
    // marks it waiting with `path_check`.
    app.person.log.push(LogEntry {
        project: Some(PROJECT),
        input: Some([5; 16]),
        job: Some(super::super::person::job_of([5; 16])),
        command: Some("make".into()),
        ..Default::default()
    });
    app.person_queue_seen();
    render(&mut app);
    assert!(app.person.log[0].path_check);
    assert_eq!(app.person_log_hint(), Some("Esc drop the command"));
    press(&mut app, KeyCode::Esc);
    let hold = take_decision(&mut app);
    assert_eq!(hold.action, Some(4), "Hold first");
    reply_to(&mut app, hold, Ok(()));
    let drop = take_decision(&mut app);
    assert_eq!(drop.action, Some(2), "then Drop");
    assert_eq!(drop.input_id.as_deref(), Some(&[5; 16][..]));
    reply_to(&mut app, drop, Ok(()));
    assert!(app.person.log[0].dropped);
    assert_eq!(app.notice, "Command dropped; it did not run");
}

#[test]
fn a_hold_too_late_says_the_command_started_and_sends_no_drop() {
    let mut app = in_log(vec![]);
    app.person.log.push(LogEntry {
        project: Some(PROJECT),
        input: Some([5; 16]),
        command: Some("make".into()),
        ..Default::default()
    });
    app.queue_entries = vec![checking(5)];
    app.person_queue_seen();
    render(&mut app);
    press(&mut app, KeyCode::Esc);
    let hold = take_decision(&mut app);
    reply_to(&mut app, hold, Err("input_not_preparing; draft retained"));
    assert_eq!(app.notice, "The command already started; /cancel stops it");
    assert!(app.queue_request.is_none());
    assert!(!app.person.log[0].dropped);
    // A failed Resume names the error.
    let mut app = in_log(vec![held(4, Vec::new())]);
    press(&mut app, KeyCode::Enter);
    let resume = take_decision(&mut app);
    reply_to(&mut app, resume, Err("input_not_held"));
    assert_eq!(app.notice, "Could not run the command: input_not_held");
    assert!(app.person.log[0].paths_held);
}

#[test]
fn enter_and_esc_on_an_ordinary_entry_keep_their_meaning() {
    let mut app = in_log(vec![queued(6, false, 3)]);
    assert_eq!(app.person_log_hint(), None);
    press(&mut app, KeyCode::Enter);
    assert!(
        app.queue_request.is_none(),
        "no decision for a running entry"
    );
    assert!(
        app.take_job_request().is_some(),
        "Enter opens the job viewer"
    );
    press(&mut app, KeyCode::Esc);
    assert!(app.queue_request.is_none());
}

/// EPG-T2 (execution-path-guard.md, EPG-T record: "In queue focus Enter on
/// a `paths_changed` entry also resumes it"): Enter on the focused held
/// entry sends Resume (action 1) for that input and keeps the draft; the
/// hint names the action; another held entry keeps its `/queue resume`
/// notice.
#[test]
fn enter_in_queue_focus_resumes_a_paths_changed_entry() {
    let mut app = app();
    let other = pb::ConversationQueueEntry {
        hold_reason: Some("secrets_lost".into()),
        ..queued(5, true, 2)
    };
    app.queue_entries = vec![
        other,
        held(4, vec![warning(kind::MODEL_CHANGED, "Makefile")]),
    ];
    app.person_queue_seen();
    render(&mut app);
    app.insert("draft text");
    app.handle(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)));
    assert_eq!(app.focus, Focus::Queue(1), "the held entry is last");
    assert!(
        app.hints().contains("↵ run the held command"),
        "{}",
        app.hints()
    );
    press(&mut app, KeyCode::Enter);
    let decision = take_decision(&mut app);
    assert_eq!(decision.action, Some(1), "Resume");
    assert_eq!(decision.input_id.as_deref(), Some(&[4; 16][..]));
    assert_eq!(app.notice, "Confirming the held command");
    reply_to(&mut app, decision, Ok(()));
    assert!(
        !app.person
            .log
            .iter()
            .any(|e| e.input == Some([4; 16]) && e.paths_held)
    );
    assert_eq!(app.draft(), "draft text", "a key decision keeps the draft");

    // The other held entry: no decision, the existing notice.
    press(&mut app, KeyCode::Up);
    assert_eq!(app.focus, Focus::Queue(0));
    assert!(app.hints().contains("↵ send now"), "{}", app.hints());
    press(&mut app, KeyCode::Enter);
    assert!(app.queue_request.is_none());
    assert_eq!(app.notice, "Held input needs /queue resume ID");
}

#[test]
fn a_busy_queue_sends_no_decision() {
    let mut app = in_log(vec![held(4, Vec::new())]);
    app.queue_busy = true;
    press(&mut app, KeyCode::Enter);
    assert!(app.queue_request.is_none());
    assert!(app.person.paths.decision.is_none());
    assert!(app.notice.starts_with("Queue request pending"));
}

#[test]
fn a_held_entry_from_another_client_gets_a_log_entry() {
    let app = in_log(vec![held(
        7,
        vec![warning(kind::MODEL_CHANGED, "Makefile")],
    )]);
    assert_eq!(app.person.log.len(), 1);
    assert_eq!(app.person.log[0].input, Some([7; 16]));
    // A queued entry that is not held or started does not.
    let app = in_log(vec![queued(8, false, 1)]);
    assert!(app.person.log.is_empty());
}

#[test]
fn the_drawn_held_entry_shows_its_lines_above_the_composer() {
    let mut app = in_log(vec![held(
        4,
        vec![warning(kind::MODEL_CHANGED, "Makefile")],
    )]);
    let rows = screen(&render(&mut app));
    let at = |text: &str| rows.iter().position(|r| r.contains(text));
    let header = at("!! make").expect("header");
    let line = at("⚠ changed by the model since your last command: Makefile").expect("warning");
    let held = at(HELD_TEXT).expect("held line");
    assert!(header < line && line < held, "{rows:#?}");
}

// ----- The restored log -----

#[test]
fn a_restored_entry_reads_its_warnings_from_the_job_detail() {
    let mut app = app();
    app.person_restored(vec![(0, [9; 16])]);
    let job = app.person.log[0].job.unwrap();
    let detail = vec![warning(kind::CHANGED_SINCE_APPROVAL, ".envrc")];
    app.person_folded(FoldUpdate {
        job,
        result: Ok(pb::JobPage::default()),
        path_warnings: Some((detail.clone(), 0)),
    });
    assert_eq!(app.person.log[0].path_warnings, detail);
    assert!(
        texts(&app.person.log[0]).contains(&"⚠ changed since you approved it: .envrc".to_owned())
    );
    assert!(!texts(&app.person.log[0]).iter().any(|t| t.contains("more")));
    // The queue's list, seen first, is kept.
    let queue = vec![warning(kind::MODEL_CHANGED, "build.rs")];
    app.person.log[0].path_warnings = queue.clone();
    app.person_folded(FoldUpdate {
        job,
        result: Ok(pb::JobPage::default()),
        path_warnings: Some((detail, 3)),
    });
    assert_eq!(app.person.log[0].path_warnings, queue);
    assert_eq!(app.person.log[0].path_overflow, 0);
}

/// EPG-T2: a restored log shows the detail's overflow count (field 7) as
/// "⚠ and N more" below its warnings, and an overflow alone still shows.
#[test]
fn a_restored_entry_shows_the_detail_overflow() {
    let mut restored = app();
    restored.person_restored(vec![(0, [9; 16])]);
    let job = restored.person.log[0].job.unwrap();
    let detail = vec![warning(kind::MODEL_CHANGED, "build.rs")];
    restored.person_folded(FoldUpdate {
        job,
        result: Ok(pb::JobPage::default()),
        path_warnings: Some((detail, 7)),
    });
    assert_eq!(restored.person.log[0].path_overflow, 7);
    let lines = texts(&restored.person.log[0]);
    let at = |text: &str| lines.iter().position(|l| l == text);
    let warning_line =
        at("⚠ changed by the model since your last command: build.rs").expect("warning");
    let more = at("⚠ and 7 more").expect("overflow line");
    assert!(warning_line < more, "{lines:#?}");

    let mut app = app();
    app.person_restored(vec![(0, [9; 16])]);
    let job = app.person.log[0].job.unwrap();
    app.person_folded(FoldUpdate {
        job,
        result: Ok(pb::JobPage::default()),
        path_warnings: Some((Vec::new(), 2)),
    });
    assert!(texts(&app.person.log[0]).contains(&"⚠ and 2 more".to_owned()));
}

// ----- `/paths` -----

#[test]
fn paths_arguments_parse_to_each_request() {
    let list = parse("/paths", PROJECT).unwrap();
    assert_eq!(
        list,
        PathGuardRequest::List(pb::PathGuardList {
            project_id: Some(PROJECT.to_vec())
        })
    );
    let PathGuardRequest::Approve(one) =
        parse("/paths approve .git/hooks/pre-commit", PROJECT).unwrap()
    else {
        panic!("approve")
    };
    assert_eq!(one.path.as_deref(), Some(".git/hooks/pre-commit"));
    assert_eq!(
        (one.all, one.request_id.as_ref().map(Vec::len)),
        (None, Some(16))
    );
    let PathGuardRequest::Approve(all) = parse("/paths approve all", PROJECT).unwrap() else {
        panic!("approve all")
    };
    assert_eq!((all.path, all.all), (None, Some(true)));
    let PathGuardRequest::Revoke(revoke) = parse("/paths revoke sub dir/.envrc", PROJECT).unwrap()
    else {
        panic!("revoke")
    };
    assert_eq!(revoke.path.as_deref(), Some("sub dir/.envrc"));
    assert_eq!(revoke.project_id.as_deref(), Some(&PROJECT[..]));
    for bad in [
        "/paths list",
        "/paths approve",
        "/paths approve   ",
        "/paths revoke",
        "/paths forget .envrc",
        "/pathsx",
    ] {
        assert_eq!(parse(bad, PROJECT), Err(USAGE), "{bad}");
    }
    let long = format!("/paths approve {}", "p".repeat(MAX_PATH_BYTES + 1));
    assert_eq!(parse(&long, PROJECT), Err(USAGE));
}

#[test]
fn paths_needs_a_project_and_capability_8() {
    let mut app = app();
    app.selected_project = None;
    run(&mut app, "/paths");
    assert_eq!(app.notice, "Select a project first; draft retained");
    assert_eq!(app.draft(), "/paths");
    assert!(app.take_path_guard_request().is_none());
    let mut app = fresh();
    app.person.supported = Some(false);
    run(&mut app, "/paths");
    assert_eq!(app.notice, crate::tui::queue::COMMANDS_NOT_ENABLED);
    assert!(app.take_path_guard_request().is_none());
    let mut app = fresh();
    run(&mut app, "/paths what");
    assert_eq!(app.notice, USAGE);
    run(&mut app, "/paths approve all");
    let request = app.take_path_guard_request().expect("request");
    assert!(request.command.mutation());
    assert_eq!(request.draft, "/paths approve all");
    assert!(app.person.paths.busy);
    // A second request waits for the first.
    run(&mut app, "/paths");
    assert_eq!(app.notice, "Path request pending; draft retained");
    assert!(app.take_path_guard_request().is_none());
    // /cancel stops the wait.
    run(&mut app, "/cancel");
    assert!(app.take_path_guard_cancel());
    assert_eq!(
        app.notice,
        "Path request wait stopped; an approval may still be applied"
    );
}
fn fresh() -> App {
    app()
}
/// Submits `draft` as typed and sent with Enter.
fn run(app: &mut App, draft: &str) {
    app.editor = Editor::new();
    app.insert(draft);
    app.submit();
}

#[test]
fn a_reply_refreshes_the_list_and_clears_the_draft() {
    let mut app = app();
    run(&mut app, "/paths approve all");
    let request = app.take_path_guard_request().unwrap();
    let approved: Vec<String> = (0..3).map(|i| format!("a{i}/.envrc")).collect();
    app.path_guard_finished(Update {
        request,
        outcome: Outcome::Reply(Box::new(pb::PathGuardReply {
            pending: vec![warning(kind::MODEL_CHANGED, "build.rs")],
            overflow: Some(2),
            approved: approved.clone(),
            error: None,
            approved_omitted: Some(253),
        })),
    });
    assert!(!app.person.paths.busy);
    assert_eq!(app.draft(), "");
    assert_eq!(app.notice, "Approved every implicit-execution file shown");
    assert_eq!(app.result_title, "Execution paths");
    assert_eq!(
        app.config_result.as_deref(),
        Some(
            "Warnings before the next command\n\
             ⚠ changed by the model since your last command: build.rs\n\
             ⚠ and 2 more\n\
             \n\
             Approved paths\n\
             a0/.envrc\n\
             a1/.envrc\n\
             a2/.envrc\n\
             and 253 more approved paths"
        )
    );
    // An empty list.
    assert_eq!(
        reply_text(&pb::PathGuardReply::default()),
        "Warnings before the next command\nNo path warnings\n\nApproved paths\nNo approved paths"
    );
    assert!(
        reply_text(&pb::PathGuardReply {
            approved_omitted: Some(1),
            ..Default::default()
        })
        .ends_with("Approved paths\nand 1 more approved path")
    );
}

#[test]
fn approve_and_revoke_notices_name_the_path() {
    let approve = parse("/paths approve .envrc", PROJECT).unwrap();
    let revoke = parse("/paths revoke .envrc", PROJECT).unwrap();
    assert_eq!(done_text(&approve), "Approved .envrc");
    assert_eq!(done_text(&revoke), "Revoked .envrc");
    assert_eq!(
        done_text(&parse("/paths", PROJECT).unwrap()),
        "Execution paths"
    );
}

#[test]
fn each_error_token_has_its_text_and_keeps_the_draft() {
    let list = parse("/paths", PROJECT).unwrap();
    let approve = parse("/paths approve .git/hooks/x", PROJECT).unwrap();
    let all = parse("/paths approve all", PROJECT).unwrap();
    let revoke = parse("/paths revoke .envrc", PROJECT).unwrap();
    assert_eq!(error_text(token::NOT_FOUND, &list), "Project not found");
    assert_eq!(error_text(token::NOT_FOUND, &all), "Project not found");
    assert_eq!(
        error_text(token::NOT_FOUND, &approve),
        "No such file in the project: .git/hooks/x"
    );
    assert_eq!(
        error_text(token::NOT_FOUND, &revoke),
        "Not approved: .envrc"
    );
    assert_eq!(
        error_text(token::PATH_INVALID, &approve),
        "Give a path inside the project, relative to its folder, without .."
    );
    assert_eq!(
        error_text(token::TOO_LARGE, &approve),
        "The file is over 1 MiB and cannot be approved; Asura warns of it before each command"
    );
    assert_eq!(
        error_text(token::NOT_IMPLICIT, &approve),
        "Only implicit-execution paths are approved: .git/hooks, .git/config, .gitattributes, \
         .gitmodules, .envrc and .cargo/config"
    );
    assert_eq!(
        error_text(token::LIMIT, &approve),
        "The project has the most approvals (256 paths, or 4,096 approvals and revocations); \
         revoke one first"
    );
    assert_eq!(
        error_text(token::BUSY, &list),
        "The path guard is busy; try again"
    );
    assert_eq!(
        error_text(token::UNAVAILABLE, &list),
        "The path guard cannot read the journal, the project folder or the file"
    );
    assert_eq!(
        error_text("denied\u{7}", &list),
        "Path request failed: denied "
    );
    for token in token::ALL {
        let mut app = app();
        run(&mut app, "/paths revoke .envrc");
        let request = app.take_path_guard_request().unwrap();
        app.path_guard_finished(Update {
            request,
            outcome: Outcome::Reply(Box::new(pb::PathGuardReply {
                error: Some(token.into()),
                ..Default::default()
            })),
        });
        assert_eq!(app.draft(), "/paths revoke .envrc", "{token}");
        assert!(app.notice.ends_with("; draft retained"), "{token}");
        assert!(app.config_result.is_none(), "{token}");
    }
}

#[test]
fn transport_failures_are_definite_or_unconfirmed() {
    let mut app = app();
    run(&mut app, "/paths approve .envrc");
    let request = app.take_path_guard_request().unwrap();
    app.path_guard_finished(Update {
        request: request.clone(),
        outcome: Outcome::Unconfirmed("outcome unconfirmed".into()),
    });
    assert_eq!(
        app.notice,
        "outcome unconfirmed; the outcome is unconfirmed; /paths shows the current list"
    );
    assert_eq!(app.draft(), "/paths approve .envrc");
    app.path_guard_finished(Update {
        request,
        outcome: Outcome::Definite(crate::tui::queue::COMMANDS_NOT_ENABLED.into()),
    });
    assert_eq!(
        app.notice,
        format!(
            "{}; draft retained",
            crate::tui::queue::COMMANDS_NOT_ENABLED
        )
    );
}
