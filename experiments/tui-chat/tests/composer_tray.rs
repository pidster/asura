//! CP1–CP3 and CP6 through real input routing, delivered projections and terminal buffers.

use asura_tui_chat::{
    app::{App, Overlay},
    model::{Action, MessageState, Project},
    ui,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend, layout::Rect};

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
fn working() -> App {
    let mut app = App::new(true);
    app.handle(Event::Paste("Initial task".into()));
    plain(&mut app, KeyCode::Enter);
    plain(&mut app, KeyCode::F(5));
    render(&mut app, 80, 24, false);
    app
}
fn direct(app: &mut App, text: &str, action: char) {
    render(app, 80, 24, false);
    app.handle(Event::Paste(text.into()));
    control(app, action);
    plain(app, KeyCode::F(5));
}

#[test]
fn cp1_empty_presented_target_allows_typing_and_shortcut_in_one_batch() {
    let mut app = working();
    app.handle(Event::Paste("Steer\nwith e\u{301} 👩🏽‍💻".into()));
    control(&mut app, 's');
    let pending = app.fixture.pending.as_ref().unwrap();
    assert_eq!(pending.action, Action::Steer);
    assert_eq!(pending.text, "Steer\nwith e\u{301} 👩🏽‍💻");
    let submitted = pending.id;
    app.handle(Event::Paste("Newer draft".into()));
    plain(&mut app, KeyCode::F(5));
    assert_eq!(app.editor.text(), "Newer draft");
    let tray = app.fixture.message_tray();
    assert_eq!(
        tray.items
            .iter()
            .filter(|item| item.id == submitted)
            .count(),
        1
    );
    assert_eq!(
        tray.items
            .iter()
            .find(|item| item.id == submitted)
            .unwrap()
            .state,
        MessageState::Acknowledged
    );
    render(&mut app, 80, 24, false);
    control(&mut app, 't');
    assert_eq!(app.fixture.pending.as_ref().unwrap().action, Action::Queue);
}

#[test]
fn cp1_shortcuts_never_target_an_unseen_successor_or_idle_task() {
    for shortcut in ['s', 't'] {
        let mut idle = App::new(true);
        idle.handle(Event::Paste("keep idle draft".into()));
        render(&mut idle, 80, 24, false);
        control(&mut idle, shortcut);
        assert!(idle.fixture.pending.is_none());
        assert_eq!(idle.editor.text(), "keep idle draft");

        let mut app = working();
        direct(&mut app, "queued successor", 't');
        render(&mut app, 80, 24, false);
        let previous = app.fixture.target();
        app.handle(Event::Paste("must not retarget".into()));
        plain(&mut app, KeyCode::F(5)); // completes and starts a different task, without a paint
        assert_ne!(app.fixture.target(), previous);
        control(&mut app, shortcut);
        assert!(app.fixture.pending.is_none());
        assert_eq!(app.editor.text(), "must not retarget");
        assert!(app.notice.contains("changed"));
        render(&mut app, 80, 24, false);
        plain(&mut app, KeyCode::F(5)); // ends without any successor
        control(&mut app, shortcut);
        assert!(app.fixture.pending.is_none());
        assert!(app.fixture.task.is_none());
        assert_eq!(app.editor.text(), "must not retarget");
    }
}

#[test]
fn cp1_rejected_shortcuts_preserve_actual_selection_cursor_and_undo() {
    let mut app = working();
    let text = "Keep e\u{301} 👩🏽‍💻";
    app.handle(Event::Paste(text.into()));
    key(&mut app, KeyCode::Left, KeyModifiers::SHIFT);
    render(&mut app, 80, 24, false);
    let cursor = app.editor.cursor();
    plain(&mut app, KeyCode::F(2));
    plain(&mut app, KeyCode::F(2)); // changed connection revision, no paint
    control(&mut app, 's');
    assert!(app.fixture.pending.is_none());
    render(&mut app, 80, 24, false);
    assert_eq!(app.editor.cursor(), cursor);
    app.handle(Event::Paste("X".into()));
    assert_eq!(app.editor.text(), "Keep e\u{301} X");
    control(&mut app, 'z');
    assert_eq!(app.editor.text(), text);
    control(&mut app, 'y');
    assert_eq!(app.editor.text(), "Keep e\u{301} X");
}

#[test]
fn cp1_repeat_release_overlay_paste_and_tiny_focus_cannot_submit() {
    let mut app = working();
    app.handle(Event::Paste("retained".into()));
    for character in ['s', 't', 'b'] {
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            app.handle(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char(character),
                KeyModifiers::CONTROL,
                kind,
            )));
            assert!(app.fixture.pending.is_none());
            assert!(app.overlay.is_none());
        }
    }
    plain(&mut app, KeyCode::F(1));
    assert_eq!(app.composer_status().characters, 8);
    assert!(app.composer_status().spinner.is_some());
    for character in ['s', 't', 'b'] {
        control(&mut app, character);
    }
    assert!(matches!(app.overlay, Some(Overlay::Help)));
    assert!(app.fixture.pending.is_none());
    plain(&mut app, KeyCode::Esc);
    control(&mut app, 's'); // opening focus cleared the old presentation
    assert!(app.fixture.pending.is_none());
    render(&mut app, 80, 24, false);
    control(&mut app, 'v');
    assert_eq!(app.composer_status().characters, 8);
    assert!(render(&mut app, 80, 24, false).contains("Ctrl+V"));
    for character in ['s', 't', 'b'] {
        control(&mut app, character);
    }
    assert!(app.paste_mode);
    assert!(app.capture_error.is_some());
    assert!(app.fixture.pending.is_none());
    control(&mut app, 'v');
    plain(&mut app, KeyCode::Esc);
    app.handle(Event::Resize(29, 7));
    control(&mut app, 's');
    assert!(app.fixture.pending.is_none());
    assert_eq!(app.editor.text(), "retained");
}

#[test]
fn cp1_status_truth_and_manual_animation_do_not_advance_work() {
    let mut app = working();
    assert_eq!(app.composer_status().characters, 0);
    assert!(app.composer_status().spinner.is_some());
    assert!(!app.advance(124));
    assert!(app.advance(125));
    assert!(!app.advance(126));
    assert!(
        !app.advance(1376),
        "a whole ten-frame cycle leaves the same frame"
    );
    assert_eq!(app.fixture.task.as_ref().unwrap().progress, 0);
    app.handle(Event::Paste("draft".into()));
    assert_eq!(app.composer_status().characters, 5);
    for (w, h) in [(120, 40), (80, 24), (40, 12), (30, 8)] {
        for light in [false, true] {
            let screen = render(&mut app, w, h, light);
            let bar = screen.lines().last().unwrap();
            assert!(bar.contains(if w < 60 { "5c" } else { "5 chars" }), "{bar}");
            assert!(bar.contains("18%"), "{bar}");
            assert!(
                bar.contains(app.composer_status().spinner.unwrap()),
                "{bar}"
            );
            assert!(!bar.contains("Steer") && !bar.contains("Queue"), "{bar}");
        }
    }
    plain(&mut app, KeyCode::F(3));
    assert!(app.composer_status().spinner.is_none());
    assert!(!app.advance(1500));
    plain(&mut app, KeyCode::F(3));
    render(&mut app, 80, 24, false);
    control(&mut app, 's');
    assert!(app.fixture.pending.is_some());
    assert!(app.composer_status().spinner.is_some());
    assert!(
        app.advance(1625),
        "delivered work still runs while Steer is pending"
    );
    plain(&mut app, KeyCode::F(7));
    let status = app.composer_status();
    assert!(status.spinner.is_none());
    assert_eq!(status.project.model.context_used_percent, None);
    assert!(!app.advance(1750));
}

#[test]
fn cp1_capacity_reason_and_protected_text_remain_visible() {
    let mut app = working();
    for i in 0..8 {
        direct(&mut app, &format!("queued {i}"), 't');
    }
    app.handle(Event::Paste("still editable".into()));
    assert!(app.composer_status().spinner.is_some());
    assert!(app.advance(125), "capacity does not pause delivered work");
    let screen = render(&mut app, 30, 8, false);
    assert!(screen.contains("capacity full"), "{screen}");
    control(&mut app, 't');
    assert!(app.fixture.pending.is_none());
    assert_eq!(app.fixture.queued.len(), 8);
    assert_eq!(app.editor.text(), "still editable");
}

#[test]
fn cp2_tray_displays_unknown_identity_once_and_hides_accepted_effects() {
    let mut app = working();
    direct(&mut app, "Visible queued text", 't');
    render(&mut app, 80, 24, false);
    app.handle(Event::Paste("Unacknowledged steer".into()));
    control(&mut app, 's');
    let id = app.fixture.pending.as_ref().unwrap().id;
    plain(&mut app, KeyCode::F(7));
    let tray = app.fixture.message_tray();
    assert_eq!(tray.items.iter().filter(|item| item.id == id).count(), 1);
    assert_eq!(tray.items[0].state, MessageState::Unknown);
    assert!(
        tray.items
            .iter()
            .filter(|item| item.id != id)
            .all(|item| item.stale)
    );
    let screen = render(&mut app, 80, 24, false);
    assert!(
        screen.contains("Steer · Unknown: Unacknowledged steer"),
        "{screen}"
    );
    assert!(!screen.contains("Steer · Acknowledged"));
    app.handle(Event::Paste("independent draft".into()));
    plain(&mut app, KeyCode::F(2));
    let tray = app.fixture.message_tray();
    assert_eq!(
        tray.items.iter().find(|item| item.id == id).unwrap().state,
        MessageState::Acknowledged
    );
    assert_eq!(app.editor.text(), "independent draft");
}

#[test]
fn cp3_list_is_captured_scoped_and_full_text_remains_pinned_after_expiry() {
    let mut app = working();
    let original = "Full Unicode e\u{301} 👩🏽‍💻 界\nsecond line";
    direct(&mut app, original, 's');
    control(&mut app, 'b');
    assert!(matches!(
        app.overlay,
        Some(Overlay::MessageList { selected: None, .. })
    ));
    plain(&mut app, KeyCode::Enter);
    assert!(matches!(
        app.overlay,
        Some(Overlay::MessageList { selected: None, .. })
    ));
    // Acknowledged steering is presented before the running turn.
    plain(&mut app, KeyCode::Tab);
    plain(&mut app, KeyCode::Enter);
    assert!(matches!(
        app.overlay,
        Some(Overlay::MessageInspector { .. })
    ));
    assert!(app.overlay_view().unwrap().detail.contains(original));
    let pinned = app.overlay.clone();
    // Model lifecycle continues while this read-only view retains its identity.
    for i in 0..12 {
        let target = app.fixture.target();
        app.fixture
            .submit_request(target, Action::Steer, 0, format!("later {i}"), 0)
            .unwrap();
        app.fixture.acknowledge_pending();
    }
    assert!(app.fixture.message_tray().history_trimmed);
    assert!(app.overlay_view().unwrap().detail.contains("Expired"));
    assert!(app.overlay_view().unwrap().detail.contains(original));
    plain(&mut app, KeyCode::Enter);
    assert_eq!(app.overlay, pinned);
    assert!(app.fixture.pending.is_none());
    control(&mut app, 'p');
    assert_eq!(app.project(), Project::Observatory);
    assert!(app.overlay.is_none());
    assert!(app.fixture.message_tray().items.is_empty());
    control(&mut app, 'b');
    assert!(app.overlay.is_none());
    assert!(!render(&mut app, 80, 24, false).contains(original));
}

#[test]
fn cp3_full_multiline_text_survives_transcript_eviction_and_scrolls_to_end() {
    let mut app = App::new(true);
    let original = format!("{}LAST 👩🏽‍💻", "wide 界 e\u{301}\n".repeat(1800));
    app.handle(Event::Paste(original.clone()));
    plain(&mut app, KeyCode::Enter);
    plain(&mut app, KeyCode::F(5));
    control(&mut app, 'b');
    let captured = app.overlay.clone();
    for i in 0..120 {
        app.fixture.toggle_decision();
        app.fixture.toggle_decision();
        app.fixture.advance(i);
    }
    assert!(app.fixture.truncated);
    assert_eq!(app.overlay, captured);
    assert!(app.overlay_view().unwrap().detail.contains(&original));
    for _ in 0..1000 {
        plain(&mut app, KeyCode::PageDown);
    }
    let screen = render(&mut app, 30, 8, false);
    assert!(screen.contains("LAST"), "{screen}");
    assert!(app.fixture.pending.is_none());
    plain(&mut app, KeyCode::Enter);
    assert_eq!(app.overlay, captured);
}

#[test]
fn cp3_translated_layout_keeps_editor_cursor_and_minimum_space() {
    for (width, height) in [(120, 40), (80, 24), (40, 12), (30, 8)] {
        for light in [false, true] {
            let mut app = working();
            app.handle(Event::Paste("draft 👩🏽‍💻\nline".into()));
            let area = Rect::new(3, 2, width, height);
            let mut terminal = Terminal::new(TestBackend::new(width + 6, height + 4)).unwrap();
            terminal
                .draw(|frame| ui::draw_in_area(frame, &mut app, light, area, " hint "))
                .unwrap();
            let (x, y) = app.editor.cursor().unwrap();
            assert!(area.contains((x, y).into()));
            let geometry = ui::app_geometry(&app, area);
            assert!(geometry.transcript.height >= 3);
            assert!(geometry.editor.height >= 1);
            assert!(geometry.editor.contains((x, y).into()));
            let buffer = terminal.backend().buffer();
            for outside in [(0, 0), (2, 2), (width + 3, height + 2)] {
                assert_eq!(buffer[outside].symbol(), " ");
            }
            if width == 30 {
                assert_eq!(geometry.tray.height, 0);
                assert_eq!(geometry.destination.height, 0);
            }
        }
    }
}

#[test]
fn cp3_collapsed_tray_preserves_unread_cue_during_work_and_decisions() {
    for project in [Project::Studio, Project::Observatory] {
        for (width, height) in [(30, 8), (80, 8)] {
            let mut app = App::new(true);
            if project == Project::Observatory {
                control(&mut app, 'p');
            }
            for index in 0..20 {
                app.fixture.submit(format!("history {index}"), 0).unwrap();
                app.fixture.acknowledge_pending();
                app.fixture.complete(0);
            }
            app.fixture.submit("active".into(), 0).unwrap();
            app.fixture.acknowledge_pending();
            direct(&mut app, "queued follow-up", 't');
            render(&mut app, width, height, false);
            plain(&mut app, KeyCode::PageUp);
            render(&mut app, width, height, false);
            assert!(!app.unread_output);
            for decision in [true, false] {
                plain(&mut app, KeyCode::F(3));
                assert_eq!(
                    app.fixture.display_task().unwrap().decision.is_some(),
                    decision
                );
                let screen = render(&mut app, width, height, false);
                let geometry = ui::app_geometry(&app, Rect::new(0, 0, width, height));
                let destination = screen
                    .lines()
                    .nth(usize::from(geometry.destination.y))
                    .unwrap();
                assert_eq!(app.composer_status().project.project, project);
                assert!(destination.contains("New output"), "{screen}");
                assert!(destination.contains("^B"), "{screen}");
                assert!(app.unread_output);
            }
        }
    }
}

#[test]
fn cp1_project_switch_clears_presentation_until_the_new_project_is_painted() {
    let mut app = working();
    app.handle(Event::Paste("Studio draft".into()));
    control(&mut app, 'p');
    control(&mut app, 'p');
    control(&mut app, 's');
    assert!(app.fixture.pending.is_none());
    assert_eq!(app.editor.text(), "Studio draft");
    render(&mut app, 80, 24, false);
    control(&mut app, 's');
    assert_eq!(app.fixture.pending.as_ref().unwrap().action, Action::Steer);
}

#[test]
fn cp1_whitespace_drafts_still_count_and_cannot_submit_then_undo_restores_count() {
    let mut app = working();
    let whitespace = " \n\u{2003}\u{00a0}";
    app.handle(Event::Paste(whitespace.into()));
    assert_eq!(app.composer_status().characters, 4);
    control(&mut app, 's');
    assert!(app.fixture.pending.is_none());
    assert_eq!(app.editor.text(), whitespace);
    assert!(app.notice.contains("Write a message"));
    key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    assert_eq!(app.composer_status().characters, 5);
    control(&mut app, 'z');
    assert_eq!(app.editor.text(), whitespace);
    assert_eq!(app.composer_status().characters, 4);
    control(&mut app, 'y');
    assert_eq!(app.composer_status().characters, 5);
}

#[test]
fn cp1_shortcut_race_error_is_visible_without_losing_decision_attention() {
    let mut app = working();
    plain(&mut app, KeyCode::F(4));
    direct(&mut app, "held text", 's');
    assert_eq!(app.fixture.recoverable.len(), 1);
    app.handle(Event::Paste("draft retained".into()));
    render(&mut app, 80, 24, false);
    plain(&mut app, KeyCode::F(3));
    control(&mut app, 's'); // target revision changed before another paint
    assert!(app.fixture.pending.is_none());
    assert_eq!(app.editor.text(), "draft retained");
    for (width, height) in [(80, 24), (30, 8)] {
        let screen = render(&mut app, width, height, false);
        assert!(screen.contains("Target changed; draft kept."), "{screen}");
        assert!(
            screen.contains("Ctrl+O") || screen.contains("^O"),
            "{screen}"
        );
    }
}

#[test]
fn cp3_pruned_history_remains_inspectable_with_relevant_overflow_cues() {
    for project in [Project::Studio, Project::Observatory] {
        let mut app = working();
        if project == Project::Observatory {
            control(&mut app, 'p');
            app.handle(Event::Paste("Observatory task".into()));
            plain(&mut app, KeyCode::Enter);
            plain(&mut app, KeyCode::F(5));
        }
        for index in 0..10 {
            direct(&mut app, &format!("receipt {index}"), 's');
        }
        for index in 0..8 {
            direct(&mut app, &format!("queue {index}"), 't');
        }
        let tray = app.fixture.message_tray();
        assert_eq!(tray.items.len(), 17);
        assert!(tray.history_trimmed);
        for (width, height) in [(60, 16), (30, 12)] {
            let geometry = ui::app_geometry(&app, Rect::new(0, 0, width, height));
            let screen = render(&mut app, width, height, false);
            let heading = screen.lines().nth(usize::from(geometry.tray.y)).unwrap();
            let hidden = 16 - usize::from(geometry.tray.height - 1);
            assert!(heading.contains(&format!("{hidden} hidden")), "{heading}");
            assert!(!heading.contains("trimmed"), "{heading}");
            assert!(
                heading.contains("^B") || heading.contains("Ctrl+B"),
                "{heading}"
            );
            assert!(heading.contains("hidden"), "{heading}");
        }
        render(&mut app, 30, 8, false);
        plain(&mut app, KeyCode::PageUp);
        plain(&mut app, KeyCode::F(3));
        let screen = render(&mut app, 30, 8, false);
        let geometry = ui::app_geometry(&app, Rect::new(0, 0, 30, 8));
        let destination = screen
            .lines()
            .nth(usize::from(geometry.destination.y))
            .unwrap();
        assert_eq!(app.composer_status().project.project, project);
        assert!(
            destination.contains("New output")
                && destination.contains("16")
                && destination.contains("^B"),
            "{screen}"
        );
        control(&mut app, 'b');
        let screen = render(&mut app, 30, 8, false);
        assert!(screen.contains("Older history trimmed."), "{screen}");
    }
}

#[test]
fn ps3_idle_and_settled_composer_has_no_permanent_rows_above_input() {
    for light in [false, true] {
        for (width, height) in [(120, 40), (80, 24), (40, 12), (30, 8)] {
            let mut app = App::new(true);
            for settled in [false, true] {
                if settled {
                    app.fixture.submit("settled input".into(), 0).unwrap();
                    app.fixture.acknowledge_pending();
                    app.fixture.complete(0);
                }
                let screen = render(&mut app, width, height, light);
                let geometry = ui::app_geometry(&app, Rect::new(0, 0, width, height));
                assert_eq!(geometry.notice.height, 0, "{screen}");
                assert_eq!(geometry.tray.height, 0, "{screen}");
                assert_eq!(geometry.destination.height, 0);
                assert_eq!(geometry.upper.y, geometry.transcript.bottom());
                assert_eq!(geometry.editor.y, geometry.upper.bottom());
                assert!(geometry.transcript.height >= 3);
                assert!(screen.lines().last().unwrap().contains("18%"));
                assert!(!screen.contains(" · Idle"));
                assert!(!screen.contains("Ready to send"));
                assert!(!screen.contains("Message tray"));
            }
            control(&mut app, 'b');
            let view = app.overlay_view().unwrap();
            assert!(view.detail.contains("settled input"));
            assert!(view.detail.contains("Completed"));
        }
    }
}

#[test]
fn cp6_inline_messages_follow_delivered_task_identity_through_reconciliation() {
    let mut app = working();
    direct(&mut app, "first refinement", 's');
    direct(&mut app, "second refinement", 's'); // a different captured revision
    direct(&mut app, "next request", 't');
    let assert_relevant = |app: &mut App| {
        let screen = render(app, 80, 24, false);
        let geometry = ui::app_geometry(app, Rect::new(0, 0, 80, 24));
        assert_eq!(geometry.tray.height, 3, "{screen}");
        let lines = screen
            .lines()
            .skip(usize::from(geometry.tray.y))
            .take(usize::from(geometry.tray.height))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(lines.contains("first refinement"), "{lines}");
        assert!(lines.contains("second refinement"), "{lines}");
        assert!(lines.contains("next request"), "{lines}");
        assert!(!lines.contains("Queue · Queued"), "{lines}");
        assert!(!lines.contains("Message tray"), "{lines}");
        assert!(!lines.contains("Initial task"), "{lines}");
    };
    assert_relevant(&mut app);
    assert!(app.fixture.set_connected(false));
    assert!(app.fixture.complete(0)); // hidden completion starts the successor
    assert_relevant(&mut app); // frozen delivered association stays intact
    assert!(app.fixture.set_connected(true));
    let screen = render(&mut app, 80, 24, false);
    let geometry = ui::app_geometry(&app, Rect::new(0, 0, 80, 24));
    assert_eq!(geometry.tray.height, 0, "{screen}");
    assert_eq!(geometry.notice.height, 0, "{screen}");
    assert_eq!(app.fixture.message_tray().items.len(), 4);
    assert!(app.fixture.complete(0));
    render(&mut app, 80, 24, false);
    control(&mut app, 'b');
    assert!(matches!(app.overlay, Some(Overlay::MessageList { .. })));
    assert_eq!(app.fixture.message_tray().items.len(), 4);
}

#[test]
fn cp6_attention_and_reading_notices_still_receive_a_row() {
    let mut app = App::new(true);
    let area = Rect::new(0, 0, 80, 24);
    assert_eq!(ui::app_geometry(&app, area).notice.height, 0);
    app.notice = "Explicit feedback".into();
    assert!(render(&mut app, 80, 24, false).contains("Explicit feedback"));
    assert_eq!(ui::app_geometry(&app, area).notice.height, 1);
    app.notice.clear();
    app.viewport.scroll_rows(-10);
    assert_eq!(ui::app_geometry(&app, area).notice.height, 1);
    assert!(render(&mut app, 80, 24, false).contains("Reading"));
    app.viewport.follow_latest();
    app.fixture.truncated = true;
    assert_eq!(ui::app_geometry(&app, area).notice.height, 1);
    assert!(render(&mut app, 80, 24, false).contains("trimmed"));
    app.fixture.truncated = false;
    app.handle(Event::Paste("pending content".into()));
    plain(&mut app, KeyCode::Enter);
    assert_eq!(ui::app_geometry(&app, area).notice.height, 1);
    assert_eq!(ui::app_geometry(&app, area).tray.height, 1);
    assert!(render(&mut app, 80, 24, false).contains("Pending: pending content"));
    plain(&mut app, KeyCode::F(7));
    assert!(render(&mut app, 80, 24, false).contains("Unknown: pending content"));
    plain(&mut app, KeyCode::F(2));
    plain(&mut app, KeyCode::F(4));
    direct(&mut app, "recover me", 't');
    let screen = render(&mut app, 80, 24, false);
    assert!(screen.contains("Held: recover me"), "{screen}");
    assert_eq!(ui::app_geometry(&app, area).notice.height, 1);
}

#[test]
fn cp6_timed_pending_keeps_independent_notices_and_one_messages_shortcut() {
    let area = Rect::new(0, 0, 80, 24);
    let mut app = App::new(false);
    app.handle(Event::Paste("timed request".into()));
    plain(&mut app, KeyCode::Enter);
    assert_eq!(ui::app_geometry(&app, area).notice.height, 0);
    app.viewport.scroll_rows(-3);
    let screen = render(&mut app, 80, 24, false);
    assert!(screen.contains("Reading earlier"), "{screen}");
    assert_eq!(ui::app_geometry(&app, area).notice.height, 1);

    let mut app = working();
    for index in 0..4 {
        direct(&mut app, &format!("follow-up {index}"), 't');
    }
    render(&mut app, 80, 24, false);
    app.handle(Event::Paste("pending follow-up".into()));
    control(&mut app, 't');
    for (width, height) in [(80, 24), (30, 8)] {
        let screen = render(&mut app, width, height, false);
        let geometry = ui::app_geometry(&app, Rect::new(0, 0, width, height));
        let composer = screen
            .lines()
            .skip(usize::from(geometry.destination.y))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            composer.matches("^B").count() + composer.matches("Ctrl+B").count(),
            1,
            "{composer}"
        );
        assert_eq!(composer.matches("F5 accepts").count(), 1, "{composer}");
    }
}

#[test]
fn cp6_minimum_notice_rows_preserve_complete_action_shortcuts() {
    for light in [false, true] {
        let mut app = working();
        app.viewport.scroll_rows(-3);
        let screen = render(&mut app, 30, 8, light);
        assert!(screen.contains("Reading earlier · Ctrl+E"), "{screen}");
        plain(&mut app, KeyCode::F(4));
        let screen = render(&mut app, 30, 8, light);
        assert!(screen.contains("Reject next · Ctrl+E latest"), "{screen}");
        app.fixture.reject_armed = false;
        app.viewport.follow_latest();
        app.fixture.truncated = true;
        let screen = render(&mut app, 30, 8, light);
        assert!(screen.contains("Earlier output trimmed."), "{screen}");
        app.fixture.truncated = false;
        plain(&mut app, KeyCode::F(3));
        let screen = render(&mut app, 30, 8, light);
        assert!(screen.contains("Decision · Ctrl+O reviews"), "{screen}");
        plain(&mut app, KeyCode::F(4));
        let screen = render(&mut app, 30, 8, light);
        assert!(screen.contains("Reject next · Ctrl+O reviews"), "{screen}");
        app.fixture.reject_armed = false;
        plain(&mut app, KeyCode::F(3));
        plain(&mut app, KeyCode::F(4));
        direct(&mut app, "recovery text", 't');
        let screen = render(&mut app, 30, 8, light);
        assert!(screen.contains("1 held · Ctrl+O recover"), "{screen}");
    }
}
