//! TP3/TP4 through actual input routing, retained editors, fixture state and rendering.

use asura_tui_chat::{app::App, model::Project, ui};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.handle(Event::Key(KeyEvent::new(code, modifiers)));
}

fn plain(app: &mut App, code: KeyCode) {
    key(app, code, KeyModifiers::NONE);
}

fn control(app: &mut App, character: char) {
    key(app, KeyCode::Char(character), KeyModifiers::CONTROL);
}

fn render(app: &mut App, width: u16, height: u16, light: bool) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| ui::draw(frame, app, light)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn ct1_command_completion_is_an_edit_and_quit_uses_the_existing_exit_guard() {
    let mut app = App::new(true);
    app.handle(Event::Paste("/qu".into()));
    plain(&mut app, KeyCode::Tab);
    assert_eq!(app.editor.text(), "/quit");
    assert!(!app.exit);
    control(&mut app, 'z');
    assert_eq!(app.editor.text(), "/qu");
    plain(&mut app, KeyCode::Tab);
    render(&mut app, 80, 24, false);
    plain(&mut app, KeyCode::Enter);
    assert!(
        app.exit,
        "an otherwise empty /quit uses the normal exit path"
    );

    let mut guarded = App::new(true);
    control(&mut guarded, 'p');
    guarded.handle(Event::Paste("Other project draft".into()));
    control(&mut guarded, 'p');
    guarded.handle(Event::Paste("/exit".into()));
    render(&mut guarded, 80, 24, false);
    plain(&mut guarded, KeyCode::Enter);
    assert!(!guarded.exit);
    assert!(render(&mut guarded, 80, 24, false).contains("Unsaved"));
    plain(&mut guarded, KeyCode::Enter); // The confirmation has no default.
    assert!(!guarded.exit);
    plain(&mut guarded, KeyCode::Esc);
    assert_eq!(guarded.editor.text(), "/exit");
    render(&mut guarded, 80, 24, false);
    plain(&mut guarded, KeyCode::Enter);
    plain(&mut guarded, KeyCode::Tab);
    plain(&mut guarded, KeyCode::Enter);
    assert!(guarded.exit);
}

#[test]
fn ct1d_ct1e_literal_quote_and_unknown_slash_never_fall_through() {
    let mut app = App::new(true);
    app.handle(Event::Paste("//tmp/file".into()));
    render(&mut app, 80, 24, false);
    plain(&mut app, KeyCode::Enter);
    assert!(app.fixture.pending.is_none());
    assert_eq!(app.editor.text(), "//tmp/file");

    control(&mut app, 'l');
    plain(&mut app, KeyCode::Tab);
    plain(&mut app, KeyCode::Enter);
    app.handle(Event::Paste("'/tmp/file".into()));
    render(&mut app, 80, 24, false);
    plain(&mut app, KeyCode::Enter);
    assert_eq!(app.fixture.pending.as_ref().unwrap().text, "'/tmp/file");
}

#[test]
fn ct5a_project_local_source_change_retains_command_text() {
    let mut app = App::new(true);
    app.handle(Event::Paste("/ext:checks/test check this".into()));
    render(&mut app, 80, 24, false);
    plain(&mut app, KeyCode::F(10));
    plain(&mut app, KeyCode::Enter);
    assert!(app.fixture.pending.is_none());
    assert_eq!(app.editor.text(), "/ext:checks/test check this");
    control(&mut app, 'p');
    assert!(app.other_fixture().pending.is_none());
    control(&mut app, 'p');
    plain(&mut app, KeyCode::F(10));
    render(&mut app, 80, 24, false);
    plain(&mut app, KeyCode::Enter);
    assert!(app.fixture.pending.is_some());
}

#[test]
fn ct8_short_extension_name_preserves_definition_identity_and_old_name_rejects() {
    let mut app = App::new(true);
    let definition = app
        .fixture
        .command_catalogue()
        .entries
        .into_iter()
        .find(|entry| entry.name == "/ext:checks/test")
        .unwrap();
    assert_eq!(definition.id, "extension:checks/test");
    assert_eq!(definition.source_id, "extension:checks");
    app.handle(Event::Paste("/ext:checks/test check this".into()));
    render(&mut app, 80, 24, false);
    plain(&mut app, KeyCode::Enter);
    let capture = app
        .fixture
        .pending
        .as_ref()
        .unwrap()
        .command
        .as_ref()
        .unwrap();
    assert_eq!(capture.definition_id, definition.id);
    assert_eq!(capture.source_id, definition.source_id);

    let mut old = App::new(true);
    old.handle(Event::Paste("/extension:checks/test check this".into()));
    render(&mut old, 80, 24, false);
    plain(&mut old, KeyCode::Enter);
    assert!(old.fixture.pending.is_none());
    assert_eq!(old.editor.text(), "/extension:checks/test check this");
}

#[test]
fn tp4_navigation_retains_actual_editor_selection_cursor_and_undo() {
    for light in [false, true] {
        let mut app = App::new(true);
        let original = "A e\u{301} 👩🏽‍💻";
        app.handle(Event::Paste(original.into()));
        render(&mut app, 80, 24, light);
        key(&mut app, KeyCode::Left, KeyModifiers::SHIFT);
        render(&mut app, 80, 24, light);
        let cursor = app.editor.cursor();
        control(&mut app, 'p');
        assert_eq!(app.project(), Project::Observatory);
        app.handle(Event::Paste("B draft".into()));
        control(&mut app, 'p');
        render(&mut app, 80, 24, light);
        assert_eq!(app.editor.text(), original);
        assert_eq!(app.editor.cursor(), cursor);
        app.handle(Event::Paste("X".into()));
        assert_eq!(app.editor.text(), "A e\u{301} X");
        control(&mut app, 'z');
        assert_eq!(app.editor.text(), original);
        control(&mut app, 'p');
        assert_eq!(app.editor.text(), "B draft");
        control(&mut app, 'z');
        assert!(app.editor.is_empty());
    }
}

#[test]
fn tp4_background_acceptance_cannot_retarget_or_clear_visible_draft() {
    let mut app = App::default();
    app.handle(Event::Paste("Studio request".into()));
    plain(&mut app, KeyCode::Enter);
    control(&mut app, 'p');
    app.handle(Event::Paste("Observatory draft".into()));
    app.advance(250);
    // A second tick also permits an explicitly separated acceptance/delivery step.
    app.advance(251);
    assert_eq!(app.project(), Project::Observatory);
    assert_eq!(app.editor.text(), "Observatory draft");
    assert!(app.fixture.pending.is_none());
    assert!(app.fixture.task.is_none());
    assert!(app.other_fixture().pending.is_none());
    assert!(app.other_fixture().task.is_some());
    assert!(
        app.other_fixture()
            .transcript
            .iter()
            .any(|s| s.text.contains("Studio request"))
    );
    for (width, height) in [(120, 40), (80, 24), (40, 12)] {
        let visible = render(&mut app, width, height, false);
        assert!(visible.contains("Observatory"), "{visible}");
        assert!(visible.contains("Observatory draft"), "{visible}");
    }
    control(&mut app, 'p');
    assert!(app.editor.is_empty());
    assert!(render(&mut app, 80, 24, false).contains("Studio request"));
}

#[test]
fn tp3_working_enter_requires_a_choice_and_keeps_newer_text_after_acceptance() {
    let mut app = App::new(true);
    app.handle(Event::Paste("Start the work".into()));
    plain(&mut app, KeyCode::Enter);
    plain(&mut app, KeyCode::F(5));
    assert!(app.fixture.task.is_some());
    app.handle(Event::Paste("Refine the work".into()));
    plain(&mut app, KeyCode::Enter);
    plain(&mut app, KeyCode::Enter);
    app.handle(Event::Paste("Cannot edit a modal choice".into()));
    assert_eq!(app.editor.text(), "Refine the work");
    assert!(app.fixture.pending.is_none());
    for light in [false, true] {
        let visible = render(&mut app, 40, 12, light);
        assert!(visible.contains("Steer"), "{visible}");
        assert!(visible.contains("Queue"), "{visible}");
    }
    plain(&mut app, KeyCode::Tab);
    plain(&mut app, KeyCode::Enter);
    assert_eq!(
        app.fixture.pending.as_ref().unwrap().text,
        "Refine the work"
    );
    app.handle(Event::Paste("Independent next draft".into()));
    plain(&mut app, KeyCode::F(5));
    assert_eq!(app.editor.text(), "Independent next draft");
    assert!(app.fixture.pending.is_none());
    let visible = render(&mut app, 80, 24, false);
    assert!(visible.contains("Independent next draft"), "{visible}");
    assert!(
        app.fixture
            .transcript
            .iter()
            .any(|s| s.text.contains("Refine the work"))
    );
}

#[test]
fn tp4_recovery_appends_after_whole_newer_draft_as_one_undo_transaction() {
    let mut app = App::new(true);
    plain(&mut app, KeyCode::F(4));
    app.handle(Event::Paste("Rejected original".into()));
    plain(&mut app, KeyCode::Enter);
    plain(&mut app, KeyCode::F(5));
    assert_eq!(app.fixture.recoverable.len(), 1);
    app.handle(Event::Paste("Newer e\u{301} 👩🏽‍💻".into()));
    control(&mut app, 'a'); // Recovery must not replace this selection.
    control(&mut app, 'o');
    plain(&mut app, KeyCode::Enter);
    assert_eq!(app.fixture.recoverable.len(), 1);
    plain(&mut app, KeyCode::Tab);
    plain(&mut app, KeyCode::Enter);
    assert_eq!(app.editor.text(), "Newer e\u{301} 👩🏽‍💻\nRejected original");
    assert!(app.fixture.recoverable.is_empty());
    assert!(app.fixture.pending.is_none());
    control(&mut app, 'z');
    assert_eq!(app.editor.text(), "Newer e\u{301} 👩🏽‍💻");
    control(&mut app, 'y');
    assert_eq!(app.editor.text(), "Newer e\u{301} 👩🏽‍💻\nRejected original");
}

#[test]
fn tp3_expired_decision_stays_focused_across_compact_and_tiny_resizes() {
    let mut app = App::new(true);
    app.handle(Event::Paste("Start".into()));
    plain(&mut app, KeyCode::Enter);
    plain(&mut app, KeyCode::F(5));
    app.handle(Event::Paste("Keep editing later".into()));
    plain(&mut app, KeyCode::F(3));
    control(&mut app, 'o');
    plain(&mut app, KeyCode::Enter);
    assert!(app.fixture.task.as_ref().unwrap().decision.is_some());
    plain(&mut app, KeyCode::F(3));
    for (width, height) in [(40, 12), (29, 7), (80, 24)] {
        let visible = render(&mut app, width, height, false);
        if width >= 30 {
            assert!(visible.contains("unavailable"), "{visible}");
        }
        plain(&mut app, KeyCode::Tab);
        plain(&mut app, KeyCode::Enter);
        assert!(app.overlay.is_some());
        assert!(app.fixture.pending.is_none());
        assert_eq!(app.editor.text(), "Keep editing later");
    }
    plain(&mut app, KeyCode::Esc);
    app.handle(Event::Paste(" now".into()));
    assert_eq!(app.editor.text(), "Keep editing later now");
}

#[test]
fn tp4_lost_ack_reconnects_to_original_accepted_request_without_resubmission() {
    let mut app = App::new(true);
    app.handle(Event::Paste("Accepted before disconnect".into()));
    plain(&mut app, KeyCode::Enter);
    let original = app.fixture.pending.as_ref().unwrap().id;
    plain(&mut app, KeyCode::F(7));
    assert!(!app.fixture.connected);
    assert!(app.fixture.pending.as_ref().unwrap().unknown);
    assert!(!render(&mut app, 80, 24, false).contains("Message received"));
    let task = app.fixture.task.as_ref().unwrap().target.id;
    app.handle(Event::Paste("Independent draft".into()));
    plain(&mut app, KeyCode::F(2));
    assert!(app.fixture.connected);
    assert!(app.fixture.pending.is_none());
    assert!(app.fixture.recoverable.is_empty());
    assert_eq!(app.fixture.task.as_ref().unwrap().target.id, task);
    assert!(!app.fixture.acknowledge(original));
    assert_eq!(app.editor.text(), "Independent draft");
    assert_eq!(
        app.fixture
            .transcript
            .iter()
            .filter(|s| s.text.contains("Accepted before disconnect"))
            .count(),
        1
    );
    assert!(render(&mut app, 80, 24, false).contains("Independent draft"));
}

#[test]
fn tp4_long_pending_text_never_borrows_a_delivered_transcript_anchor() {
    for reject in [false, true] {
        let mut app = App::new(true);
        for index in 0..30 {
            app.fixture
                .submit(format!("Retained entry {index}"), 0)
                .unwrap();
            app.fixture.acknowledge_pending();
            app.fixture.complete(0);
        }
        app.viewport.scroll_rows(-12);
        let before = render(&mut app, 80, 24, false);
        let anchored: Vec<_> = before.lines().skip(1).take(8).collect();
        if reject {
            plain(&mut app, KeyCode::F(4));
        }
        let pending = format!("{}FINAL PENDING MARKER", "pending line\n".repeat(100));
        app.handle(Event::Paste(pending.clone()));
        plain(&mut app, KeyCode::Enter);
        let waiting = render(&mut app, 80, 24, false);
        assert_eq!(
            waiting.lines().skip(1).take(8).collect::<Vec<_>>(),
            anchored
        );
        control(&mut app, 'o');
        assert!(app.overlay_view().unwrap().detail.ends_with(&pending));
        // The inspector retains its identity and focus after resolution.
        plain(&mut app, KeyCode::F(5));
        plain(&mut app, KeyCode::Enter);
        assert!(
            app.overlay_view()
                .unwrap()
                .detail
                .contains("Captured request resolved")
        );
        assert!(app.fixture.pending.is_none());
        plain(&mut app, KeyCode::Esc);
        let resolved = render(&mut app, 80, 24, false);
        assert_eq!(
            resolved.lines().skip(1).take(8).collect::<Vec<_>>(),
            anchored
        );
        assert!(!app.notice.contains("trimmed"));
    }
}
