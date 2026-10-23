//! J4T-11 (view part) and the composer-interactions validation of the job
//! list and job viewer: each focus transition, key routing, narrow layouts
//! and the hint text, against fixture replies (no I/O).
use super::*;
use ratatui::{Terminal, backend::TestBackend};

const EPOCH: &str = "e";
const HERE: [u8; 16] = [1; 16];
const THERE: [u8; 16] = [2; 16];

fn fixture() -> App {
    let mut app = App::new();
    app.view.connection = "connected";
    app.view.installation = "graph_ready";
    app.view.epoch = Some(EPOCH.into());
    app.projects = Some(vec![
        pb::ProjectReply {
            project_id: Some(HERE.to_vec()),
            name: Some("here".into()),
            current: Some(true),
            ..Default::default()
        },
        pb::ProjectReply {
            project_id: Some(THERE.to_vec()),
            name: Some("there".into()),
            current: Some(true),
            ..Default::default()
        },
    ]);
    app.selected_project = Some(HERE);
    app
}
fn id(n: u8) -> [u8; 16] {
    let mut id = [n; 16];
    id[0] = 0xa0 + n;
    id
}
fn entry(n: u8, project: [u8; 16], state: pb::JobState) -> pb::JobEntry {
    pb::JobEntry {
        job_id: Some(id(n).to_vec()),
        project_id: Some(project.to_vec()),
        conversation_id: Some(vec![3; 16]),
        origin: Some(pb::JobOrigin::Model as i32),
        command: Some(format!("cargo test {n}")),
        state: Some(state as i32),
        stdout_bytes: Some(2048),
        stderr_bytes: Some(0),
        sequence: Some(100 - u64::from(n)),
        ..Default::default()
    }
}
fn press(app: &mut App, code: KeyCode) {
    app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}
fn ch(app: &mut App, c: char) {
    press(app, KeyCode::Char(c));
}
fn run(app: &mut App, draft: &str) {
    app.editor = Editor::new();
    app.insert(draft);
    app.submit();
}
fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    buffer
        .content
        .chunks(usize::from(width))
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
/// The dispatcher's half: take the queued request.
fn sent(app: &mut App) -> Request {
    app.take_job_request().expect("a job request was queued")
}
fn reply(app: &mut App, request: Request, reply: Reply) {
    app.jobs_finished(Finished {
        request,
        result: Ok(reply),
    });
}
fn list_reply(entries: Vec<pb::JobEntry>, next: Option<u64>) -> Reply {
    Reply::List(pb::JobListReply {
        entries,
        next_before: next,
        revision: Some(7),
        live_in_scope: Some(1),
        live_in_service: Some(1),
        execution_enabled: Some(true),
        error: None,
    })
}
fn page(n: u8, stream: Stream, start: u64, text: &str, total: u64, state: pb::JobState) -> Reply {
    let mut job = entry(n, HERE, state);
    job.stdout_bytes = Some(total);
    Reply::Page(pb::JobPage {
        entry: Some(job),
        pages: vec![pb::JobStreamPage {
            stream: Some(stream.code()),
            start: Some(start),
            text: Some(text.into()),
            next_offset: Some(start + text.len() as u64),
            total_bytes: Some(total),
            state: Some(if state == pb::JobState::Settled {
                pb::JobPageState::Complete
            } else {
                pb::JobPageState::Live
            } as i32),
        }],
        error: None,
    })
}
fn numbered(from: usize, count: usize) -> String {
    (from..from + count)
        .map(|n| format!("line {n}\n"))
        .collect()
}
/// A list panel showing entries 1–3 of this project.
fn listed(app: &mut App, next: Option<u64>) {
    run(app, "/jobs");
    let request = sent(app);
    reply(
        app,
        request,
        list_reply(
            vec![
                entry(1, HERE, pb::JobState::Running),
                entry(2, HERE, pb::JobState::Settled),
                entry(3, HERE, pb::JobState::Queued),
            ],
            next,
        ),
    );
}

#[test]
fn cf12_jobs_command_validates_gates_and_keeps_the_draft() {
    let mut app = fixture();
    run(&mut app, "/jobs mine");
    assert_eq!(app.notice, "Use /jobs or /jobs all; draft retained");
    assert_eq!(app.draft(), "/jobs mine");
    assert!(app.take_job_request().is_none());

    let mut none = app_without_project();
    run(&mut none, "/jobs");
    assert_eq!(none.notice, "Select a project first; draft retained");
    assert!(none.take_job_request().is_none());
    // /jobs all needs no selected project.
    run(&mut none, "/jobs all");
    assert!(matches!(
        sent(&mut none).command,
        Command::List {
            all: true,
            before: None,
            ..
        }
    ));

    let mut gone = fixture();
    gone.view.connection = "unavailable";
    run(&mut gone, "/jobs");
    assert_eq!(gone.notice, "Service not connected; draft retained");

    run(&mut app, "/jobs");
    assert_eq!(app.notice, "Job list pending · Esc stops waiting");
    let request = sent(&mut app);
    assert_eq!(
        request.command,
        Command::List {
            all: false,
            project: Some(HERE),
            before: None
        }
    );
    assert_eq!(request.epoch, EPOCH);
    assert!(!app.jobs.open(), "the panel opens on the reply");
    // A second command while the request slot is busy.
    run(&mut app, "/job cancel 00000000000000000000000000000077");
    assert_eq!(app.notice, PENDING);
    assert_eq!(app.draft(), "/job cancel 00000000000000000000000000000077");
    assert!(app.take_job_request().is_none());
}
fn app_without_project() -> App {
    let mut app = fixture();
    app.selected_project = None;
    app
}

#[test]
fn en1_a_service_without_jobs_shows_nothing_job_related() {
    let mut app = fixture();
    run(&mut app, "/jobs");
    let request = sent(&mut app);
    app.jobs_finished(Finished {
        request,
        result: Err(Failure::Unsupported),
    });
    assert_eq!(app.notice, UNSUPPORTED);
    assert!(!app.jobs.open());
    // Later commands answer at once and send nothing.
    for draft in ["/jobs", "/jobs all", "/job abcdef", "/job pin abcdef"] {
        app.notice.clear();
        run(&mut app, draft);
        assert_eq!(app.notice, UNSUPPORTED, "{draft}");
        assert!(app.take_job_request().is_none());
    }
    // The status slot's answer hides the status items.
    let scope = app.job_status_scope().unwrap();
    app.jobs_status(StatusUpdate {
        scope,
        result: Err(Failure::Unsupported),
    });
    assert_eq!(app.jobs_status_text(), "");
    app.notice.clear();
    assert!(!render(&mut app, 100, 20).contains("jobs "));
    // Response navigation offers no job key.
    app.append_response("p".into(), Some(vec![9; 16]));
    app.response_selection = Some(0);
    assert!(!app.hints().contains("j job"));
    ch(&mut app, 'j');
    assert_eq!(app.notice, UNSUPPORTED);
    assert!(app.take_job_request().is_none());
}

#[test]
fn cf12_list_panel_rows_paging_and_keys() {
    let mut app = fixture();
    listed(&mut app, Some(98));
    assert!(app.jobs.open());
    assert_eq!(app.draft(), "", "the submitted draft clears on the reply");
    assert_eq!(app.jobs.cache.len(), 3);
    let screen = render(&mut app, 120, 30);
    assert!(screen.contains("Jobs · this project · page 1 · more · Esc closes"));
    assert!(screen.contains(&worker::short(&id(1))));
    assert!(screen.contains("cargo test 1 · running · — · out 2.0 KiB err 0 B"));
    assert_eq!(
        app.hints(),
        "↑↓ select · ↵ view · c cancel · p pin · d delete · n note · PgDn more · Esc close"
    );
    // Selection.
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.jobs.list.as_ref().unwrap().selected, 2);
    // Down past the last row loads the next page with `before`.
    press(&mut app, KeyCode::Down);
    let request = sent(&mut app);
    assert!(matches!(
        request.command,
        Command::List {
            before: Some(98),
            ..
        }
    ));
    assert_eq!(app.jobs.list.as_ref().unwrap().state, ListState::Loading);
    reply(
        &mut app,
        request,
        list_reply(vec![entry(4, HERE, pb::JobState::Settled)], None),
    );
    let list = app.jobs.list.as_ref().unwrap();
    assert_eq!((list.cursors.len(), list.selected), (2, 0));
    assert!(render(&mut app, 120, 30).contains("page 2 · Esc closes"));
    // Up at the top goes back to the previous page.
    press(&mut app, KeyCode::Up);
    let request = sent(&mut app);
    assert!(matches!(
        request.command,
        Command::List { before: None, .. }
    ));
    reply(
        &mut app,
        request,
        list_reply(
            vec![
                entry(1, HERE, pb::JobState::Running),
                entry(2, HERE, pb::JobState::Settled),
            ],
            Some(98),
        ),
    );
    let list = app.jobs.list.as_ref().unwrap();
    assert_eq!((list.cursors.len(), list.selected), (1, 1));
    // invalid_cursor reloads from the newest page.
    press(&mut app, KeyCode::PageDown);
    let request = sent(&mut app);
    reply(
        &mut app,
        request,
        Reply::List(pb::JobListReply {
            error: Some("invalid_cursor".into()),
            ..Default::default()
        }),
    );
    let request = sent(&mut app);
    assert!(matches!(
        request.command,
        Command::List { before: None, .. }
    ));
    // busy: the panel shows the error and r retries.
    reply(
        &mut app,
        request,
        Reply::List(pb::JobListReply {
            error: Some("busy".into()),
            ..Default::default()
        }),
    );
    assert!(render(&mut app, 120, 30).contains("Job list error: busy"));
    assert_eq!(app.hints(), "r retry · Esc close");
    ch(&mut app, 'r');
    let request = sent(&mut app);
    reply(
        &mut app,
        request,
        list_reply(vec![entry(1, HERE, pb::JobState::Running)], None),
    );
    assert_eq!(app.jobs.list.as_ref().unwrap().state, ListState::Ready);
    // Escape closes; nothing is sent.
    press(&mut app, KeyCode::Esc);
    assert!(!app.jobs.open());
    assert!(app.take_job_request().is_none());
}

#[test]
fn cf12_all_projects_rows_name_their_project_and_escape_stops_waiting() {
    let mut app = fixture();
    run(&mut app, "/jobs all");
    // Escape before the reply stops the wait; nothing opens.
    press(&mut app, KeyCode::Esc);
    assert!(!app.jobs.open());
    assert_eq!(app.notice, "Job request cancelled");
    assert!(app.take_job_request().is_none(), "never sent");
    run(&mut app, "/jobs all");
    let request = sent(&mut app);
    // /cancel while it waits: the worker reports the stopped read.
    app.editor = Editor::new();
    app.insert("/cancel");
    app.submit();
    assert!(app.take_jobs_cancel());
    app.jobs_finished(Finished {
        request,
        result: Err(Failure::Cancelled),
    });
    assert!(app.jobs.list.is_none());
    run(&mut app, "/jobs all");
    let request = sent(&mut app);
    reply(
        &mut app,
        request,
        list_reply(
            vec![
                entry(1, HERE, pb::JobState::Running),
                entry(2, THERE, pb::JobState::Running),
            ],
            None,
        ),
    );
    let screen = render(&mut app, 120, 30);
    assert!(screen.contains("Jobs · all projects"));
    assert!(screen.contains("[here]"));
    assert!(screen.contains("[there]"));
}

#[test]
fn composer_list_to_viewer_and_back_to_the_list() {
    let mut app = fixture();
    listed(&mut app, None);
    press(&mut app, KeyCode::Enter);
    let request = sent(&mut app);
    assert_eq!(
        request.command,
        Command::Read {
            job: id(1),
            project: Some(HERE),
            stream: Stream::Stdout,
            offset: Some(0),
        }
    );
    let viewer = app.jobs.viewer.as_ref().unwrap();
    assert_eq!(
        (viewer.origin, &viewer.phase),
        (Origin::List, &Phase::Loading)
    );
    assert_eq!(app.hints(), "Loading · Esc close");
    reply(
        &mut app,
        request,
        page(
            1,
            Stream::Stdout,
            0,
            "hello\nworld\n",
            12,
            pb::JobState::Running,
        ),
    );
    assert_eq!(app.jobs.viewer.as_ref().unwrap().phase, Phase::Viewing);
    let screen = render(&mut app, 120, 30);
    assert!(
        screen.contains(&format!(
            "Job {} · running · stdout · bytes 0–12 of 12 · live",
            worker::short(&id(1))
        )),
        "{screen}"
    );
    assert!(screen.contains("hello") && screen.contains("world"));
    assert_eq!(
        app.hints(),
        "Tab stream · ↑↓ PgUp/PgDn scroll · Home/End · f follow · i detail · Esc close"
    );
    press(&mut app, KeyCode::Esc);
    assert!(app.jobs.viewer.is_none());
    assert!(app.jobs.open(), "the list stays open under the viewer");
    press(&mut app, KeyCode::Esc);
    assert!(!app.jobs.open());
}

#[test]
fn ts4_viewer_streams_scroll_pages_follow_and_detail() {
    let mut app = fixture();
    app.jobs.supported = Some(true);
    let handle = worker::handle(&id(1));
    run(&mut app, &format!("/job {handle}"));
    let request = sent(&mut app);
    assert!(matches!(
        request.command,
        Command::Read {
            project: None,
            offset: Some(0),
            stream: Stream::Stdout,
            ..
        }
    ));
    let first = numbered(0, 40);
    let total = 20_000;
    reply(
        &mut app,
        request,
        page(1, Stream::Stdout, 0, &first, total, pb::JobState::Running),
    );
    assert_eq!(app.draft(), "", "the command draft clears on the page");
    render(&mut app, 100, 24);
    let rows = app.jobs.viewer.as_ref().unwrap().rows;
    assert!(rows > 1 && rows < 40);
    // Down scrolls within the page.
    press(&mut app, KeyCode::Down);
    assert_eq!(app.jobs.viewer.as_ref().unwrap().view().line, 1);
    assert!(render(&mut app, 100, 24).contains("line 1"));
    // PageDown to the bottom, then past it: the next page at next_offset.
    for _ in 0..4 {
        press(&mut app, KeyCode::PageDown);
    }
    let request = sent(&mut app);
    let next = first.len() as u64;
    assert!(matches!(request.command, Command::Read { offset: Some(o), .. } if o == next));
    reply(
        &mut app,
        request,
        page(
            1,
            Stream::Stdout,
            next,
            &numbered(40, 40),
            total,
            pb::JobState::Running,
        ),
    );
    let viewer = app.jobs.viewer.as_ref().unwrap();
    assert_eq!((viewer.view().current, viewer.view().line), (Some(next), 0));
    // Up at the top of a page returns to the cached previous page: no request.
    press(&mut app, KeyCode::Up);
    assert!(app.take_job_request().is_none());
    assert_eq!(app.jobs.viewer.as_ref().unwrap().view().current, Some(0));
    render(&mut app, 100, 24);
    assert!(
        app.jobs.viewer.as_ref().unwrap().view().line > 0,
        "lands at the bottom"
    );
    // Home uses the cache too.
    press(&mut app, KeyCode::Home);
    assert!(app.take_job_request().is_none());
    assert_eq!(app.jobs.viewer.as_ref().unwrap().view().line, 0);
    // Tab: stderr opens at byte 0; Tab back keeps stdout's position.
    press(&mut app, KeyCode::Tab);
    let request = sent(&mut app);
    assert!(matches!(
        request.command,
        Command::Read {
            stream: Stream::Stderr,
            offset: Some(0),
            ..
        }
    ));
    reply(
        &mut app,
        request,
        page(1, Stream::Stderr, 0, "warning\n", 8, pb::JobState::Running),
    );
    assert!(render(&mut app, 100, 24).contains("stderr · bytes 0–8 of 8"));
    press(&mut app, KeyCode::Tab);
    assert!(app.take_job_request().is_none(), "stdout kept its page");
    assert_eq!(app.jobs.viewer.as_ref().unwrap().stream, Stream::Stdout);
    // End reads the last page and lands at its bottom.
    press(&mut app, KeyCode::End);
    let request = sent(&mut app);
    assert!(matches!(
        request.command,
        Command::Read { offset: None, .. }
    ));
    reply(
        &mut app,
        request,
        page(
            1,
            Stream::Stdout,
            19_000,
            &numbered(500, 30),
            total,
            pb::JobState::Running,
        ),
    );
    render(&mut app, 100, 24);
    assert!(render(&mut app, 100, 24).contains("line 529"));
    // f follows: one observe session from the held end.
    ch(&mut app, 'f');
    let follow = app.job_follow().unwrap();
    assert_eq!(follow.stream, Stream::Stdout);
    assert_eq!(follow.offset, 19_000 + numbered(500, 30).len() as u64);
    assert_eq!(follow.after_state, Some(pb::JobState::Running as i32));
    assert!(render(&mut app, 100, 24).contains("· following"));
    assert!(app.hints().starts_with("f stop following"));
    // New bytes keep the view at the end.
    let end = follow.offset;
    let Reply::Page(grown) = page(
        1,
        Stream::Stdout,
        end,
        "fresh output\n",
        end + 13,
        pb::JobState::Running,
    ) else {
        unreachable!()
    };
    assert!(app.jobs_followed(FollowUpdate {
        session: follow.session,
        result: Ok(grown),
    }));
    assert!(render(&mut app, 100, 24).contains("fresh output"));
    // A stale session's page is ignored.
    assert!(!app.jobs_followed(FollowUpdate {
        session: follow.session + 9,
        result: Ok(pb::JobPage::default()),
    }));
    // Up stops following (TJ-S4) and scrolls back into the cached page.
    press(&mut app, KeyCode::Up);
    assert!(app.take_job_request().is_none());
    assert_eq!(app.jobs.viewer.as_ref().unwrap().phase, Phase::Viewing);
    assert_eq!(
        app.jobs.viewer.as_ref().unwrap().view().current,
        Some(19_000)
    );
    assert!(app.job_follow().is_none());
    // Follow again; settlement returns to viewing.
    ch(&mut app, 'f');
    let follow = app.job_follow().unwrap();
    let Reply::Page(done) = page(
        1,
        Stream::Stdout,
        end + 13,
        "",
        end + 13,
        pb::JobState::Settled,
    ) else {
        unreachable!()
    };
    app.jobs_followed(FollowUpdate {
        session: follow.session,
        result: Ok(done),
    });
    assert_eq!(app.jobs.viewer.as_ref().unwrap().phase, Phase::Viewing);
    assert!(app.job_follow().is_none());
    ch(&mut app, 'f');
    assert_eq!(app.notice, "The job has settled; nothing to follow");
    // i opens the detail panel from JobInspect; Escape returns.
    ch(&mut app, 'i');
    let request = sent(&mut app);
    assert_eq!(
        request.command,
        Command::Inspect {
            job: id(1),
            project: None
        }
    );
    assert!(render(&mut app, 100, 24).contains("Loading…"));
    let mut settled = entry(1, HERE, pb::JobState::Settled);
    settled.outcome = Some(1);
    settled.exit_kind = Some(1);
    settled.exit_value = Some(0);
    reply(
        &mut app,
        request,
        Reply::Detail(pb::JobDetail {
            entry: Some(settled),
            command: Some("cargo test\n  --all".into()),
            cwd: Some("/work".into()),
            notes: vec![
                pb::JobNote {
                    author: Some(1),
                    unix_ms: Some(0),
                    text: Some("flaky".into()),
                },
                pb::JobNote {
                    author: Some(2),
                    unix_ms: Some(0),
                    text: Some("rerun passed".into()),
                },
            ],
            error: None,
            path_warnings: Vec::new(),
        }),
    );
    let screen = render(&mut app, 100, 24);
    for text in [
        "detail · Esc returns",
        "State: exit 0",
        "Cwd: /work",
        "cargo test",
        "--all",
        "Notes: 2",
        "you · —: flaky",
        "model · —: rerun passed",
    ] {
        assert!(screen.contains(text), "{text}\n{screen}");
    }
    assert_eq!(app.hints(), "↑↓ scroll · Esc viewer");
    press(&mut app, KeyCode::Esc);
    assert!(app.jobs.viewer.as_ref().unwrap().detail.is_none());
    // Escape closes the viewer opened from a command: back to the editor.
    press(&mut app, KeyCode::Esc);
    assert!(!app.jobs.open());
    assert!(app.job_follow().is_none());
}

#[test]
fn cf13_viewer_errors_retry_and_escape_while_loading() {
    let mut app = fixture();
    let handle = worker::handle(&id(1));
    run(&mut app, &format!("/job {handle}"));
    let request = sent(&mut app);
    reply(
        &mut app,
        request,
        Reply::Page(pb::JobPage {
            error: Some("busy".into()),
            ..Default::default()
        }),
    );
    assert!(
        matches!(&app.jobs.viewer.as_ref().unwrap().phase, Phase::Failed(e) if e.starts_with("busy"))
    );
    assert!(render(&mut app, 100, 24).contains("Error: busy"));
    assert_eq!(app.draft(), format!("/job {handle}"), "draft kept on error");
    assert_eq!(app.hints(), "r retry · Esc close");
    ch(&mut app, 'r');
    let request = sent(&mut app);
    assert!(matches!(
        request.command,
        Command::Read {
            offset: Some(0),
            ..
        }
    ));
    // A lost connection shows the error; the job is unaffected.
    app.jobs_finished(Finished {
        request,
        result: Err(Failure::Unavailable("service_unavailable".into())),
    });
    assert!(
        matches!(&app.jobs.viewer.as_ref().unwrap().phase, Phase::Failed(e) if e == "service_unavailable")
    );
    ch(&mut app, 'r');
    let request = sent(&mut app);
    reply(
        &mut app,
        request,
        Reply::Page(pb::JobPage {
            error: Some("not_found".into()),
            ..Default::default()
        }),
    );
    assert!(render(&mut app, 100, 24).contains("Error: job not found"));
    press(&mut app, KeyCode::Esc);
    assert!(!app.jobs.open());
    // Escape while loading closes the viewer and stops the wait.
    run(&mut app, &format!("/job {handle}"));
    let _request = sent(&mut app);
    press(&mut app, KeyCode::Esc);
    assert!(app.jobs.viewer.is_none());
    assert!(app.take_jobs_cancel());
}

#[test]
fn cf13_cancel_confirms_another_projects_job_and_reports_each_outcome() {
    let mut app = fixture();
    listed(&mut app, None);
    // This project's job: sent at once with the expected project.
    ch(&mut app, 'c');
    let request = sent(&mut app);
    let Command::Cancel {
        job, project, kill, ..
    } = request.command.clone()
    else {
        panic!("{request:?}")
    };
    assert_eq!((job, project, kill), (id(1), HERE, false));
    let mut stopping = entry(1, HERE, pb::JobState::Stopping);
    stopping.cancel_requested = Some(true);
    reply(
        &mut app,
        request,
        Reply::Cancel(pb::JobCancelReply {
            request_id: None,
            entry: Some(stopping),
            requested: Some(true),
            error: None,
        }),
    );
    assert_eq!(
        app.notice,
        format!(
            "Stopping job {}; /jobs shows the settlement",
            worker::short(&id(1))
        )
    );
    assert!(render(&mut app, 120, 30).contains("stopping · cancel requested"));
    // A settled job: requested false with its result.
    press(&mut app, KeyCode::Down);
    ch(&mut app, 'c');
    let request = sent(&mut app);
    let mut settled = entry(2, HERE, pb::JobState::Settled);
    settled.outcome = Some(1);
    settled.exit_kind = Some(1);
    settled.exit_value = Some(3);
    reply(
        &mut app,
        request,
        Reply::Cancel(pb::JobCancelReply {
            request_id: None,
            entry: Some(settled),
            requested: Some(false),
            error: None,
        }),
    );
    assert_eq!(
        app.notice,
        format!("Job {} already settled: exit 3", worker::short(&id(2)))
    );
    press(&mut app, KeyCode::Esc);

    // Another project's job asks first; n and Escape send nothing.
    let mut app = fixture();
    run(&mut app, "/jobs all");
    let request = sent(&mut app);
    reply(
        &mut app,
        request,
        list_reply(vec![entry(5, THERE, pb::JobState::Running)], None),
    );
    for no in [KeyCode::Char('n'), KeyCode::Esc] {
        ch(&mut app, 'c');
        let screen = render(&mut app, 120, 30);
        assert!(
            screen.contains(&format!(
                "Cancel job {} in project there? (y/n)",
                worker::short(&id(5))
            )),
            "{screen}"
        );
        assert_eq!(app.hints(), "y confirm · n or Esc cancel");
        // Other keys keep the prompt.
        press(&mut app, KeyCode::Down);
        assert!(app.jobs.confirm.is_some());
        press(&mut app, no);
        assert!(app.jobs.confirm.is_none());
        assert_eq!(app.notice, "Nothing sent");
        assert!(app.take_job_request().is_none());
        assert!(app.jobs.list.is_some(), "the list keeps focus");
    }
    ch(&mut app, 'c');
    ch(&mut app, 'y');
    let request = sent(&mut app);
    assert!(matches!(request.command, Command::Cancel { project, .. } if project == THERE));
    // Errors change nothing; a lost reply is unconfirmed and safe to repeat.
    reply(
        &mut app,
        request.clone(),
        Reply::Cancel(pb::JobCancelReply {
            error: Some("project_mismatch".into()),
            ..Default::default()
        }),
    );
    assert_eq!(
        app.notice,
        "Cancel refused: the job belongs to another project; nothing changed"
    );
    app.jobs.pending = Some(request.clone());
    app.jobs_finished(Finished {
        request,
        result: Err(Failure::Unconfirmed),
    });
    assert_eq!(
        app.notice,
        "Cancel outcome unconfirmed; repeating /job cancel is safe"
    );
}

#[test]
fn cf13_unlisted_handle_is_inspected_before_cancel() {
    let mut app = fixture();
    let handle = worker::handle(&id(8));
    run(&mut app, &format!("/job cancel {handle}"));
    let request = sent(&mut app);
    assert_eq!(
        request.command,
        Command::Inspect {
            job: id(8),
            project: None
        }
    );
    reply(
        &mut app,
        request,
        Reply::Detail(pb::JobDetail {
            entry: Some(entry(8, THERE, pb::JobState::Running)),
            ..Default::default()
        }),
    );
    // Another project's job: the confirmation names it.
    assert!(
        app.jobs
            .confirm
            .as_ref()
            .unwrap()
            .text
            .contains("in project there")
    );
    ch(&mut app, 'y');
    let request = sent(&mut app);
    assert!(matches!(request.command, Command::Cancel { project, .. } if project == THERE));
    assert_eq!(
        request.draft.as_deref(),
        Some(&*format!("/job cancel {handle}"))
    );
    // not_found while looking up.
    app.jobs.pending = None;
    run(&mut app, &format!("/job pin {}", worker::handle(&id(9))));
    let request = sent(&mut app);
    reply(
        &mut app,
        request,
        Reply::Detail(pb::JobDetail {
            error: Some("not_found".into()),
            ..Default::default()
        }),
    );
    assert_eq!(app.notice, "Job request refused: job not found");
    assert!(app.take_job_request().is_none());
}

#[test]
fn j4e7_command_ids_resolve_by_handle_or_unique_prefix() {
    let mut app = fixture();
    listed(&mut app, None);
    press(&mut app, KeyCode::Esc);
    // Entries 1-3 share no 6-digit prefix: each resolves.
    let prefix = &worker::handle(&id(2))[..6];
    run(&mut app, &format!("/job pin {prefix}"));
    let request = sent(&mut app);
    assert!(
        matches!(request.command, Command::Retain { job, action: Retain::Pin, .. } if job == id(2))
    );
    app.jobs.pending = None;
    run(&mut app, "/job ffffff");
    assert_eq!(app.notice, NO_MATCH);
    run(&mut app, "/job abc");
    assert_eq!(app.notice, JOB_USAGE);
    run(&mut app, "/job pin");
    assert_eq!(app.notice, JOB_USAGE);
    run(&mut app, &format!("/job pin {prefix} extra"));
    assert_eq!(app.notice, JOB_USAGE);
    // Two cached jobs with one prefix: the matches are listed.
    let mut twin = entry(2, HERE, pb::JobState::Running);
    let mut other = id(2);
    other[15] = 0xff;
    twin.job_id = Some(other.to_vec());
    app.jobs.remember(&twin);
    run(&mut app, &format!("/job {prefix}"));
    assert!(app.notice.starts_with("Several jobs match: "));
    assert!(app.notice.ends_with("; draft retained"));
    assert!(app.take_job_request().is_none());
}

#[test]
fn cf14_retention_commands_confirm_delete_and_report_outcomes() {
    let mut app = fixture();
    listed(&mut app, None);
    // p pins an unpinned job.
    ch(&mut app, 'p');
    let request = sent(&mut app);
    assert!(
        matches!(request.command, Command::Retain { action: Retain::Pin, project, .. } if project == HERE)
    );
    let mut pinned = entry(1, HERE, pb::JobState::Running);
    pinned.pinned = Some(true);
    reply(
        &mut app,
        request,
        Reply::Retain(pb::JobRetainReply {
            entry: Some(pinned),
            changed: Some(true),
            ..Default::default()
        }),
    );
    let short = worker::short(&id(1));
    assert_eq!(app.notice, format!("Pinned job {short}"));
    assert!(render(&mut app, 120, 30).contains("· pinned"));
    // p again unpins; an unchanged reply says so.
    ch(&mut app, 'p');
    let request = sent(&mut app);
    assert!(matches!(
        request.command,
        Command::Retain {
            action: Retain::Unpin,
            ..
        }
    ));
    reply(
        &mut app,
        request,
        Reply::Retain(pb::JobRetainReply {
            changed: Some(false),
            ..Default::default()
        }),
    );
    assert_eq!(app.notice, format!("Job {short} is not pinned"));
    // d always confirms; a live job's output cannot be deleted.
    ch(&mut app, 'd');
    assert_eq!(
        app.jobs.confirm.as_ref().unwrap().text,
        format!("Delete the output of job {short}? Its summary and notes stay. (y/n)")
    );
    ch(&mut app, 'y');
    let request = sent(&mut app);
    assert!(matches!(
        request.command,
        Command::Retain {
            action: Retain::Delete,
            ..
        }
    ));
    reply(
        &mut app,
        request,
        Reply::Retain(pb::JobRetainReply {
            error: Some("not_settled".into()),
            ..Default::default()
        }),
    );
    assert_eq!(
        app.notice,
        format!("Job {short}: a running job's output cannot be deleted; draft retained")
    );
    // n opens the one-line note editor; Escape discards it.
    ch(&mut app, 'n');
    assert_eq!(app.hints(), "↵ add note · Esc discard");
    for c in "flaky".chars() {
        ch(&mut app, c);
    }
    assert!(render(&mut app, 120, 30).contains(&format!("Note for job {short}: flaky")));
    press(&mut app, KeyCode::Esc);
    assert!(app.jobs.list.as_ref().unwrap().note.is_none());
    assert!(app.take_job_request().is_none());
    // Enter sends the note; an empty note is refused locally.
    ch(&mut app, 'n');
    press(&mut app, KeyCode::Enter);
    assert!(app.notice.starts_with("A note is 1 to 2048 bytes"));
    for c in "ok".chars() {
        ch(&mut app, c);
    }
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Enter);
    let request = sent(&mut app);
    assert!(
        matches!(&request.command, Command::Retain { action: Retain::Note(t), .. } if t == "o")
    );
    reply(
        &mut app,
        request.clone(),
        Reply::Retain(pb::JobRetainReply {
            error: Some("notes_full".into()),
            ..Default::default()
        }),
    );
    assert!(app.notice.contains("16 notes"));
    assert!(
        app.jobs.list.as_ref().unwrap().note.is_some(),
        "the note is kept"
    );
    app.jobs.pending = Some(request.clone());
    app.jobs_finished(Finished {
        request,
        result: Err(Failure::Unconfirmed),
    });
    assert_eq!(
        app.notice,
        "Note outcome unconfirmed; check /job ID before repeating the note"
    );
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);

    // /job note TEXT is the rest of the line; /job delete confirms.
    let handle = worker::handle(&id(2));
    run(&mut app, &format!("/job note {handle}   needs  a rerun "));
    let request = sent(&mut app);
    assert!(
        matches!(&request.command, Command::Retain { action: Retain::Note(t), .. } if t == "needs  a rerun")
    );
    reply(
        &mut app,
        request,
        Reply::Retain(pb::JobRetainReply {
            changed: Some(true),
            ..Default::default()
        }),
    );
    assert_eq!(
        app.notice,
        format!("Note added to job {}", worker::short(&id(2)))
    );
    assert_eq!(app.draft(), "", "success clears the draft");
    run(&mut app, &format!("/job note {handle}"));
    assert!(app.notice.starts_with("A note is 1 to 2048 bytes"));
    assert!(app.take_job_request().is_none());
    run(&mut app, &format!("/job delete {handle}"));
    assert!(app.jobs.confirm.is_some());
    ch(&mut app, 'y');
    let request = sent(&mut app);
    reply(
        &mut app,
        request,
        Reply::Retain(pb::JobRetainReply {
            changed: Some(true),
            ..Default::default()
        }),
    );
    assert_eq!(
        app.notice,
        format!("Deleted the output of job {}", worker::short(&id(2)))
    );
}

#[test]
fn deleted_output_viewer_points_to_the_summary_and_notes() {
    let mut app = fixture();
    run(&mut app, &format!("/job {}", worker::handle(&id(1))));
    let request = sent(&mut app);
    reply(
        &mut app,
        request,
        Reply::Page(pb::JobPage {
            entry: Some(entry(1, HERE, pb::JobState::Settled)),
            pages: vec![pb::JobStreamPage {
                stream: Some(Stream::Stdout.code()),
                start: Some(0),
                text: Some(String::new()),
                next_offset: Some(0),
                total_bytes: Some(0),
                state: Some(pb::JobPageState::Deleted as i32),
            }],
            error: None,
        }),
    );
    let screen = render(&mut app, 100, 24);
    assert!(screen.contains("· deleted"));
    assert!(screen.contains("Output deleted; i shows the summary and notes"));
}

#[test]
fn ex12_outlived_notice_follows_the_terminal_event_and_restore() {
    let mut app = fixture();
    app.append_response("Run it".into(), Some(vec![4; 16]));
    let terminal = pb::ConversationEvent {
        operation_id: Some(vec![4; 16]),
        generation: Some(1),
        cursor: Some(3),
        kind: Some(5),
        text: Some("stopped".into()),
        outlived_jobs: Some(2),
        outlived_job_ids: vec![id(1).to_vec(), id(2).to_vec()],
        ..Default::default()
    };
    app.conversation_finished(crate::tui::conversation::Update {
        event: Some(terminal.clone()),
        done: true,
        ..Default::default()
    });
    let notice = "↳ 2 jobs kept running after this turn · /jobs to view or cancel";
    assert!(render(&mut app, 100, 24).contains(notice));
    // A live event carries no count.
    let mut live = App::new();
    live.append_response("Run".into(), Some(vec![4; 16]));
    live.conversation_finished(crate::tui::conversation::Update {
        event: Some(pb::ConversationEvent {
            kind: Some(2),
            outlived_jobs: Some(2),
            ..terminal.clone()
        }),
        ..Default::default()
    });
    assert!(!render(&mut live, 100, 24).contains("kept running"));
    // Drawn again from the terminal event after a restore.
    let mut restored = fixture();
    restored.apply_restoration(crate::tui::conversation::Restored {
        conversation_id: Some(vec![3; 16]),
        generation: 1,
        turns: vec![crate::tui::conversation::RestoredTurn {
            prompt: "Run it".into(),
            event: terminal,
        }],
    });
    assert!(render(&mut restored, 100, 24).contains(notice));
}

#[test]
fn activity_row_j_opens_the_viewer_or_a_chooser_and_returns_to_history() {
    let tool = |ordinal: u32, job: Option<[u8; 16]>| pb::ToolProgress {
        ordinal: Some(ordinal),
        name: Some("exec_cmd".into()),
        state: Some(2),
        job_id: job.map(|j| j.to_vec()),
        ..Default::default()
    };
    let event = |tools| pb::ConversationEvent {
        operation_id: Some(vec![4; 16]),
        generation: Some(1),
        cursor: Some(2),
        kind: Some(3),
        tools,
        ..Default::default()
    };
    let mut app = fixture();
    app.append_response("Run".into(), Some(vec![4; 16]));
    app.conversation_finished(crate::tui::conversation::Update {
        event: Some(event(vec![
            tool(1, None),
            tool(2, Some(id(1))),
            tool(3, Some(id(1))),
        ])),
        done: true,
        ..Default::default()
    });
    press(&mut app, KeyCode::F(6));
    assert!(app.hints().contains("· j job"));
    ch(&mut app, 'j');
    let request = sent(&mut app);
    assert!(matches!(request.command, Command::Read { job, .. } if job == id(1)));
    assert_eq!(app.jobs.viewer.as_ref().unwrap().origin, Origin::History);
    press(&mut app, KeyCode::Esc);
    assert!(
        app.take_jobs_cancel(),
        "closing while loading stops the read"
    );
    assert!(app.jobs.viewer.is_none());
    assert_eq!(
        app.response_selection,
        Some(0),
        "back to response navigation"
    );
    app.jobs.pending = None;

    // Several jobs: a chooser; Enter opens the selected one.
    let mut app = fixture();
    app.append_response("Run".into(), Some(vec![4; 16]));
    app.conversation_finished(crate::tui::conversation::Update {
        event: Some(event(
            (1..=11).map(|n| tool(n, Some(id(n as u8)))).collect(),
        )),
        done: true,
        ..Default::default()
    });
    press(&mut app, KeyCode::F(6));
    ch(&mut app, 'j');
    assert_eq!(
        app.jobs.chooser.as_ref().unwrap().ids.len(),
        worker::MAX_CHOOSER
    );
    assert_eq!(app.hints(), "↑↓ select · ↵ view · Esc back");
    assert!(render(&mut app, 100, 24).contains("Jobs of this response"));
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    let request = sent(&mut app);
    assert!(matches!(request.command, Command::Read { job, .. } if job == id(2)));
    assert!(app.jobs.chooser.is_none());
    // Escape from the chooser returns to response navigation.
    let mut app2 = app;
    app2.jobs.viewer = None;
    app2.jobs.pending = None;
    ch(&mut app2, 'j');
    press(&mut app2, KeyCode::Esc);
    assert!(app2.jobs.chooser.is_none());
    assert_eq!(app2.response_selection, Some(0));
    // A response without jobs.
    let mut plain = fixture();
    plain.append_response("Hi".into(), Some(vec![4; 16]));
    plain.conversation_finished(crate::tui::conversation::Update {
        event: Some(event(vec![tool(1, None)])),
        done: true,
        ..Default::default()
    });
    press(&mut plain, KeyCode::F(6));
    assert!(!plain.hints().contains("j job"));
    ch(&mut plain, 'j');
    assert_eq!(plain.notice, "No job in this response");
}

#[test]
fn es_12_status_row_counts_live_jobs_and_shows_exec_off() {
    let mut app = fixture();
    let scope = app.job_status_scope().unwrap();
    let status = |live, execution| StatusUpdate {
        scope: scope.clone(),
        result: Ok(pb::JobListReply {
            revision: Some(3),
            live_in_scope: Some(live),
            live_in_service: Some(live + 1),
            execution_enabled: Some(execution),
            ..Default::default()
        }),
    };
    assert!(app.jobs_status(status(2, true)));
    assert_eq!(app.jobs_status_text(), "jobs 2");
    let screen = render(&mut app, 120, 24);
    assert!(screen.contains("jobs 2 ·"), "{screen}");
    app.jobs_status(status(0, false));
    assert!(render(&mut app, 120, 24).contains("exec off ·"));
    app.jobs_status(status(1, false));
    assert_eq!(app.jobs_status_text(), "jobs 1 · exec off");
    // Below 40 columns there is no room for it.
    assert!(!render(&mut app, 39, 20).contains("exec off"));
    // Another project's update is ignored; a selection change hides the
    // facts until the new scope reports.
    let foreign = StatusUpdate {
        scope: Scope {
            project: THERE,
            epoch: EPOCH.into(),
        },
        result: Ok(pb::JobListReply::default()),
    };
    assert!(!app.jobs_status(foreign));
    app.selected_project = Some(THERE);
    assert_eq!(app.jobs_status_text(), "");
    app.selected_project = Some(HERE);
    // A failed status attachment shows nothing rather than a stale count.
    app.jobs_status(StatusUpdate {
        scope: scope.clone(),
        result: Err(Failure::Unavailable("service_unavailable".into())),
    });
    assert_eq!(app.jobs_status_text(), "");
    // A new service epoch forgets the capability and the counts.
    app.jobs_status(status(4, false));
    app.jobs.list = None;
    app.set_view(View {
        epoch: Some("other".into()),
        ..app.view.clone()
    });
    assert_eq!(app.jobs.supported, None);
    assert_eq!(app.jobs_status_text(), "");
}

#[test]
fn status_revision_refreshes_an_open_list() {
    let mut app = fixture();
    listed(&mut app, None);
    let scope = app.job_status_scope().unwrap();
    let status = |revision| StatusUpdate {
        scope: scope.clone(),
        result: Ok(pb::JobListReply {
            revision: Some(revision),
            live_in_scope: Some(1),
            live_in_service: Some(1),
            execution_enabled: Some(true),
            ..Default::default()
        }),
    };
    // The list's own revision: nothing to reload.
    app.jobs_status(status(7));
    assert!(app.take_job_request().is_none());
    app.jobs_status(status(8));
    let request = sent(&mut app);
    assert!(request.draft.is_none());
    assert!(matches!(
        request.command,
        Command::List { before: None, .. }
    ));
    // Silent while the slot is busy; the next change reloads.
    app.jobs_status(status(9));
    assert!(app.take_job_request().is_none());
}

#[test]
fn exit_confirmation_counts_live_jobs_of_a_tui_launched_backend() {
    let details = |owned| ServiceDetails {
        owned,
        uptime_ms: 0,
        stored_memory: Default::default(),
        thinking_level: None,
    };
    let mut app = fixture();
    app.view.service = Some(details(true));
    let scope = app.job_status_scope().unwrap();
    app.jobs_status(StatusUpdate {
        scope,
        result: Ok(pb::JobListReply {
            revision: Some(1),
            live_in_scope: Some(1),
            live_in_service: Some(3),
            execution_enabled: Some(true),
            ..Default::default()
        }),
    });
    // Ctrl+Q with an empty draft asks first.
    assert!(!app.handle(Event::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::CONTROL
    ))));
    let screen = render(&mut app, 120, 30);
    assert!(screen.contains("Exit?"));
    assert!(
        screen.contains(
            "3 jobs are still running in all projects; they stop when the service stops."
        )
    );
    press(&mut app, KeyCode::Esc);
    // /quit also asks.
    run(&mut app, "/quit");
    assert!(matches!(app.overlay, Some(Overlay::Exit(false))));
    app.overlay = None;
    // An attached (not launched) backend keeps running: no line, no prompt.
    app.view.service = Some(details(false));
    assert!(app.jobs_exit_line().is_none());
    assert!(app.handle(Event::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::CONTROL
    ))));
}

#[test]
fn job_views_route_keys_and_keep_ctrl_chords() {
    let mut app = fixture();
    listed(&mut app, None);
    // Typing goes to the view, never the composer; paste is ignored.
    ch(&mut app, 'x');
    app.handle(Event::Paste("pasted".into()));
    assert_eq!(app.draft(), "");
    // Ctrl+Q still reaches the exit rule.
    assert!(app.handle(Event::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::CONTROL
    ))));
    // The editor shows no cursor while a view is open.
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(app.jobs.open());
}

#[test]
fn narrow_layouts_keep_rows_and_headers_readable() {
    let mut app = fixture();
    listed(&mut app, None);
    let screen = render(&mut app, 44, 16);
    let short = worker::short(&id(1));
    assert!(screen.contains(&format!("{short} running · —")), "{screen}");
    press(&mut app, KeyCode::Enter);
    let request = sent(&mut app);
    reply(
        &mut app,
        request,
        page(
            1,
            Stream::Stdout,
            0,
            &numbered(0, 30),
            400,
            pb::JobState::Running,
        ),
    );
    let screen = render(&mut app, 44, 16);
    assert!(screen.contains(&format!("Job {short}")), "{screen}");
    assert!(screen.contains("line 0"));
    // Too small to draw: nothing panics and keys still work.
    render(&mut app, 20, 6);
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    assert!(!app.jobs.open());
}

#[test]
fn viewer_keeps_at_most_four_pages_per_stream() {
    let mut view = StreamView::default();
    for n in 0..6u64 {
        view.insert(pb::JobStreamPage {
            start: Some(n * 8192),
            next_offset: Some((n + 1) * 8192),
            ..Default::default()
        });
    }
    assert_eq!(view.cached(), worker::MAX_PAGES_PER_STREAM);
    // The pages farthest from the newest are dropped.
    assert!(view.find(0).is_none() && view.find(8192).is_none());
    assert!(view.find(5 * 8192).is_some());
    assert_eq!(view.before(5 * 8192).and_then(|p| p.start), Some(4 * 8192));
    // The same start replaces its page.
    view.insert(pb::JobStreamPage {
        start: Some(5 * 8192),
        text: Some("new".into()),
        ..Default::default()
    });
    assert_eq!(view.cached(), 4);
    assert_eq!(
        page_lines(&pb::JobStreamPage {
            text: Some("a\nb\n".into()),
            ..Default::default()
        }),
        vec!["a", "b"]
    );
    assert!(page_lines(&pb::JobStreamPage::default()).is_empty());
    assert_eq!(
        page_lines(&pb::JobStreamPage {
            text: Some("bell\u{7}".into()),
            ..Default::default()
        }),
        vec!["bell "]
    );
}

#[test]
fn replies_for_another_epoch_or_a_closed_view_change_nothing() {
    let mut app = fixture();
    let handle = worker::handle(&id(1));
    run(&mut app, &format!("/job {handle}"));
    let request = sent(&mut app);
    app.view.epoch = Some("later".into());
    reply(
        &mut app,
        request,
        page(1, Stream::Stdout, 0, "x\n", 2, pb::JobState::Running),
    );
    assert!(app.jobs.pending.is_none(), "the slot is free again");
    assert_eq!(app.jobs.viewer.as_ref().unwrap().phase, Phase::Loading);
    // A page for a job whose viewer closed is dropped.
    let mut app = fixture();
    run(&mut app, &format!("/job {handle}"));
    let request = sent(&mut app);
    app.jobs.viewer = None;
    reply(
        &mut app,
        request,
        page(1, Stream::Stdout, 0, "x\n", 2, pb::JobState::Running),
    );
    assert!(!app.jobs.open());
}
