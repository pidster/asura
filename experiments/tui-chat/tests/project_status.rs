//! PS1–PS4 through the canonical editor, fixture, client and renderer.

use asura_tui_chat::{app::App, model::Project, ui};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.handle(Event::Key(KeyEvent::new(code, modifiers)));
}

fn plain(app: &mut App, code: KeyCode) {
    key(app, code, KeyModifiers::NONE);
}

fn control(app: &mut App, ch: char) {
    key(app, KeyCode::Char(ch), KeyModifiers::CONTROL);
}

fn render(app: &mut App, light: bool) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    let completed = terminal.draw(|frame| ui::draw(frame, app, light)).unwrap();
    (0..40)
        .map(|y| {
            (0..120)
                .map(|x| completed.buffer[(x, y)].symbol())
                .collect()
        })
        .collect()
}

#[test]
fn ps1_four_states_keep_project_model_and_unicode_count_in_one_footer() {
    for light in [false, true] {
        let mut app = App::new(true);
        let rows = render(&mut app, light);
        let bar = rows.last().unwrap();
        assert!(
            bar.starts_with(" Studio · ~/src/studio · main · +24-8"),
            "{bar}"
        );
        assert!(bar.trim_end().ends_with("18% used · demo v1 fast"), "{bar}");
        assert!(!bar.contains("chars"));
        assert!(app.composer_status().spinner.is_none());
        assert!(
            !rows
                .iter()
                .any(|row| row.contains("Studio / Conversation 1"))
        );

        app.handle(Event::Paste("e\u{301} 👩🏽‍💻\n界".into()));
        let rows = render(&mut app, light);
        assert!(rows.last().unwrap().contains(" · 5 chars"));
        assert_eq!(app.composer_status().characters, 5);
        plain(&mut app, KeyCode::Enter);
        assert_eq!(app.composer_status().characters, 0);
        assert!(
            app.composer_status().spinner.is_none(),
            "acceptance has not arrived"
        );
        plain(&mut app, KeyCode::F(5));
        let rows = render(&mut app, light);
        assert!(
            rows.last()
                .unwrap()
                .contains(app.composer_status().spinner.unwrap())
        );
        assert!(!rows.last().unwrap().contains("chars"));

        app.handle(Event::Paste("new draft".into()));
        let rows = render(&mut app, light);
        let bar = rows.last().unwrap();
        assert!(bar.contains(" · 9 chars"), "{bar}");
        assert!(
            bar.contains(app.composer_status().spinner.unwrap()),
            "{bar}"
        );
        assert!(bar.trim_end().ends_with("18% used · demo v1 fast"));
        plain(&mut app, KeyCode::F(3));
        assert!(app.composer_status().spinner.is_none());
        assert!(render(&mut app, light).join("\n").contains("Ctrl+O"));
        plain(&mut app, KeyCode::F(3));
        plain(&mut app, KeyCode::F(5));
        assert!(app.composer_status().spinner.is_none());
        assert_eq!(app.editor.text(), "new draft");
    }
}

#[test]
fn ps2_switch_and_reconnect_preserve_scoped_metadata_and_draft_counts() {
    let mut app = App::new(true);
    app.handle(Event::Paste("one 👩🏽‍💻".into()));
    control(&mut app, 'p');
    let rows = render(&mut app, false);
    let bar = rows.last().unwrap();
    assert!(bar.contains("Observatory · ~/src/observatory"), "{bar}");
    assert!(bar.contains("feature/chat · +103-21 · rebase"), "{bar}");
    assert!(bar.trim_end().ends_with("42% used · demo v2"), "{bar}");
    assert!(!bar.contains("fast"));
    assert_eq!(app.composer_status().characters, 0);
    app.handle(Event::Paste("other".into()));
    control(&mut app, 'p');
    assert_eq!(app.composer_status().project.project, Project::Studio);
    assert_eq!(app.composer_status().characters, 5);
    plain(&mut app, KeyCode::F(2));
    let rows = render(&mut app, false);
    let bar = rows.last().unwrap();
    assert!(bar.contains(" · git ?"), "{bar}");
    assert!(bar.contains("?% used · demo v1 fast"), "{bar}");
    assert!(bar.contains("5 chars"), "{bar}");
    assert!(rows.join("\n").contains("Disconnected · F2 reconnects"));
    plain(&mut app, KeyCode::F(2));
    let rows = render(&mut app, false);
    assert!(rows.last().unwrap().contains("18% used"));
    assert_eq!(app.editor.text(), "one 👩🏽‍💻");
    control(&mut app, 'p');
    assert_eq!(app.editor.text(), "other");
    assert_eq!(app.composer_status().characters, 5);
}

#[test]
fn ps4_focus_and_paste_capture_do_not_stop_delivered_activity_or_count_staged_text() {
    let mut app = App::new(true);
    app.handle(Event::Paste("task".into()));
    plain(&mut app, KeyCode::Enter);
    plain(&mut app, KeyCode::F(5));
    app.handle(Event::Paste("draft".into()));
    plain(&mut app, KeyCode::F(1));
    assert!(app.advance(125));
    assert_eq!(app.composer_status().characters, 5);
    let rows = render(&mut app, false);
    assert!(
        rows.last()
            .unwrap()
            .contains(app.composer_status().spinner.unwrap())
    );
    assert!(rows.last().unwrap().contains("18% used"));
    plain(&mut app, KeyCode::F(1));
    control(&mut app, 'v');
    app.handle(Event::Paste("staged\ntext".into()));
    assert!(app.advance(250));
    assert_eq!(app.composer_status().characters, 5);
    let rows = render(&mut app, false);
    assert!(rows.join("\n").contains("Ctrl+V"));
    assert!(rows.last().unwrap().contains("5 chars"));
    control(&mut app, 'v');
    plain(&mut app, KeyCode::Esc);
    assert_eq!(app.editor.text(), "draft");
    assert_eq!(app.fixture.task.as_ref().unwrap().progress, 0);
}
