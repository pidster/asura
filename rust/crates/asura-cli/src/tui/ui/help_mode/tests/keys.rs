//! HP4: one fixture lists the key chords each area's router handles. The guide
//! comparison (HU4) and the router test both read it, so a guide change without
//! a routing change, or the reverse, fails a test.
use super::*;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug)]
enum Setup {
    /// Single-row draft with the cursor inside it and undo history.
    Draft,
    Empty,
    /// A draft with one earlier entry in recall.
    Recall,
    Recalling,
    /// A draft whose last edit was undone.
    Undone,
    /// A draft with a conversation log and a queue.
    WithLog,
    WithQueue,
    LogFocus,
    Browser,
    QueueMiddle,
    QueueFirst,
    QueueLast,
    QueueRejected,
    StatusProject,
    StatusModel,
    ProjectList,
    ModelList,
    /// Help mode with a query, the selection on the third result.
    HelpQuery,
    /// Help mode with the cursor at byte 0.
    HelpStart,
}

struct Chord {
    section: &'static str,
    label: &'static str,
    code: KeyCode,
    modifiers: KeyModifiers,
    setup: Setup,
}
const N: KeyModifiers = KeyModifiers::NONE;
const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;
const ALT: KeyModifiers = KeyModifiers::ALT;
const fn chord(
    section: &'static str,
    label: &'static str,
    code: KeyCode,
    modifiers: KeyModifiers,
    setup: Setup,
) -> Chord {
    Chord {
        section,
        label,
        code,
        modifiers,
        setup,
    }
}
use Setup::*;
const ANYWHERE: &str = "keys-anywhere";
const BOX: &str = "input-box-keys";
const QUEUE: &str = "queue-keys";
const LOG: &str = "conversation-log-keys";
const BROWSER: &str = "response-browser-keys";
const STATUS: &str = "status-bar-keys";
const LISTS: &str = "project-and-model-list-keys";
const HELP: &str = "help-mode-keys";

const FIXTURE: &[Chord] = &[
    chord(ANYWHERE, "Ctrl+Q", KeyCode::Char('q'), CTRL, Draft),
    chord(ANYWHERE, "Ctrl+C", KeyCode::Char('c'), CTRL, Draft),
    chord(ANYWHERE, "Ctrl+D", KeyCode::Char('d'), CTRL, Empty),
    chord(ANYWHERE, "Ctrl+S", KeyCode::Char('s'), CTRL, Draft),
    chord(ANYWHERE, "Ctrl+T", KeyCode::Char('t'), CTRL, Draft),
    chord(ANYWHERE, "Ctrl+V", KeyCode::Char('v'), CTRL, Draft),
    chord(ANYWHERE, "Ctrl+P", KeyCode::Char('p'), CTRL, Recall),
    chord(ANYWHERE, "Ctrl+N", KeyCode::Char('n'), CTRL, Recalling),
    chord(ANYWHERE, "F6", KeyCode::F(6), N, WithLog),
    chord(BOX, "Enter", KeyCode::Enter, N, Draft),
    chord(BOX, "Option+Enter", KeyCode::Enter, ALT, Draft),
    chord(BOX, "Alt+Enter", KeyCode::Enter, ALT, Draft),
    chord(BOX, "Up", KeyCode::Up, N, Recall),
    chord(BOX, "Down", KeyCode::Down, N, Draft),
    chord(BOX, "Down while recalling", KeyCode::Down, N, Recalling),
    chord(BOX, "Shift+Up", KeyCode::Up, SHIFT, WithLog),
    chord(BOX, "Shift+Down", KeyCode::Down, SHIFT, Draft),
    chord(BOX, "Ctrl+Up", KeyCode::Up, CTRL, WithQueue),
    chord(BOX, "Ctrl+Down", KeyCode::Down, CTRL, Draft),
    chord(BOX, "Left", KeyCode::Left, N, Draft),
    chord(BOX, "Right", KeyCode::Right, N, Draft),
    chord(BOX, "Shift+Left", KeyCode::Left, SHIFT, Draft),
    chord(BOX, "Shift+Right", KeyCode::Right, SHIFT, Draft),
    chord(BOX, "Option+Left", KeyCode::Left, ALT, Draft),
    chord(BOX, "Option+Right", KeyCode::Right, ALT, Draft),
    chord(BOX, "Ctrl+Left", KeyCode::Left, CTRL, Draft),
    chord(BOX, "Ctrl+Right", KeyCode::Right, CTRL, Draft),
    chord(BOX, "Alt+B", KeyCode::Char('b'), ALT, Draft),
    chord(BOX, "Alt+F", KeyCode::Char('f'), ALT, Draft),
    chord(BOX, "Home", KeyCode::Home, N, Draft),
    chord(BOX, "End", KeyCode::End, N, Draft),
    chord(BOX, "Ctrl+Home", KeyCode::Home, CTRL, Draft),
    chord(BOX, "Ctrl+End", KeyCode::End, CTRL, Draft),
    chord(BOX, "Backspace", KeyCode::Backspace, N, Draft),
    chord(BOX, "Delete", KeyCode::Delete, N, Draft),
    chord(BOX, "Option+Backspace", KeyCode::Backspace, ALT, Draft),
    chord(BOX, "Alt+Backspace", KeyCode::Backspace, ALT, Draft),
    chord(BOX, "Ctrl+A", KeyCode::Char('a'), CTRL, Draft),
    chord(BOX, "Ctrl+Z", KeyCode::Char('z'), CTRL, Draft),
    chord(BOX, "Ctrl+Y", KeyCode::Char('y'), CTRL, Undone),
    chord(BOX, "Tab", KeyCode::Tab, N, Draft),
    chord(BOX, "F1", KeyCode::F(1), N, Draft),
    chord(BOX, "?", KeyCode::Char('?'), N, Empty),
    chord(QUEUE, "Up", KeyCode::Up, N, QueueMiddle),
    chord(QUEUE, "Down", KeyCode::Down, N, QueueMiddle),
    chord(QUEUE, "Home", KeyCode::Home, N, QueueMiddle),
    chord(QUEUE, "End", KeyCode::End, N, QueueMiddle),
    chord(QUEUE, "Up on the first message", KeyCode::Up, N, QueueFirst),
    chord(
        QUEUE,
        "Down on the last message",
        KeyCode::Down,
        N,
        QueueLast,
    ),
    chord(QUEUE, "[", KeyCode::Char('['), N, QueueMiddle),
    chord(QUEUE, "]", KeyCode::Char(']'), N, QueueMiddle),
    chord(QUEUE, "Enter", KeyCode::Enter, N, QueueMiddle),
    chord(
        QUEUE,
        "Enter on a Rejected local message",
        KeyCode::Enter,
        N,
        QueueRejected,
    ),
    chord(QUEUE, "Shift+Up", KeyCode::Up, SHIFT, QueueFirst),
    chord(QUEUE, "Shift+Down", KeyCode::Down, SHIFT, QueueMiddle),
    chord(QUEUE, "Escape", KeyCode::Esc, N, QueueMiddle),
    chord(LOG, "Up", KeyCode::Up, N, LogFocus),
    chord(LOG, "Down", KeyCode::Down, N, LogFocus),
    chord(LOG, "Page Up", KeyCode::PageUp, N, LogFocus),
    chord(LOG, "Page Down", KeyCode::PageDown, N, LogFocus),
    chord(LOG, "Home", KeyCode::Home, N, LogFocus),
    chord(LOG, "End", KeyCode::End, N, LogFocus),
    chord(LOG, "Shift+Down", KeyCode::Down, SHIFT, LogFocus),
    chord(LOG, "Escape", KeyCode::Esc, N, LogFocus),
    chord(BROWSER, "Up", KeyCode::Up, N, Browser),
    chord(BROWSER, "Down", KeyCode::Down, N, Browser),
    chord(BROWSER, "Home", KeyCode::Home, N, Browser),
    chord(BROWSER, "End", KeyCode::End, N, Browser),
    chord(BROWSER, "Enter", KeyCode::Enter, N, Browser),
    chord(BROWSER, "Space", KeyCode::Char(' '), N, Browser),
    chord(BROWSER, "Page Up", KeyCode::PageUp, N, Browser),
    chord(BROWSER, "Page Down", KeyCode::PageDown, N, Browser),
    chord(BROWSER, "Escape", KeyCode::Esc, N, Browser),
    chord(BROWSER, "F6", KeyCode::F(6), N, Browser),
    chord(STATUS, "Left", KeyCode::Left, N, StatusProject),
    chord(STATUS, "Right", KeyCode::Right, N, StatusProject),
    chord(STATUS, "Tab", KeyCode::Tab, N, StatusProject),
    chord(STATUS, "Enter", KeyCode::Enter, N, StatusProject),
    chord(STATUS, "Enter", KeyCode::Enter, N, StatusModel),
    chord(STATUS, "Up", KeyCode::Up, N, StatusProject),
    chord(STATUS, "Shift+Up", KeyCode::Up, SHIFT, StatusProject),
    chord(STATUS, "Escape", KeyCode::Esc, N, StatusProject),
    chord(LISTS, "Up", KeyCode::Up, N, ProjectList),
    chord(LISTS, "Down", KeyCode::Down, N, ProjectList),
    chord(LISTS, "Home", KeyCode::Home, N, ProjectList),
    chord(LISTS, "End", KeyCode::End, N, ProjectList),
    chord(LISTS, "Enter", KeyCode::Enter, N, ProjectList),
    chord(LISTS, "Escape", KeyCode::Esc, N, ProjectList),
    chord(LISTS, "Up", KeyCode::Up, N, ModelList),
    chord(LISTS, "Down", KeyCode::Down, N, ModelList),
    chord(LISTS, "Home", KeyCode::Home, N, ModelList),
    chord(LISTS, "End", KeyCode::End, N, ModelList),
    chord(LISTS, "Enter", KeyCode::Enter, N, ModelList),
    chord(LISTS, "Escape", KeyCode::Esc, N, ModelList),
    chord(HELP, "Up", KeyCode::Up, N, HelpQuery),
    chord(HELP, "Down", KeyCode::Down, N, HelpQuery),
    chord(HELP, "Page Up", KeyCode::PageUp, N, HelpQuery),
    chord(HELP, "Page Down", KeyCode::PageDown, N, HelpQuery),
    chord(HELP, "Enter", KeyCode::Enter, N, HelpQuery),
    chord(
        HELP,
        "Backspace at the start",
        KeyCode::Backspace,
        N,
        HelpStart,
    ),
    chord(HELP, "Escape", KeyCode::Esc, N, HelpQuery),
    chord(HELP, "Ctrl+S", KeyCode::Char('s'), CTRL, HelpQuery),
    chord(HELP, "Ctrl+T", KeyCode::Char('t'), CTRL, HelpQuery),
    chord(HELP, "Ctrl+D", KeyCode::Char('d'), CTRL, HelpQuery),
    chord(HELP, "F1", KeyCode::F(1), N, HelpQuery),
];

/// Chords named by a key cell: `A or B` and `A, B or C` name separate keys.
fn chords(cell: &str) -> Vec<String> {
    cell.replace(", ", " or ")
        .split(" or ")
        .map(str::to_owned)
        .collect()
}

#[test]
fn hu4_fixture_matches_the_guide_key_cells_for_every_area() {
    let index = help::index().unwrap();
    // The four areas' key sections, and the help mode section the guide adds.
    let sections: Vec<(String, &str)> = Area::ALL
        .into_iter()
        .flat_map(|area| {
            area.sections()
                .iter()
                .filter(|id| id.starts_with("keys#"))
                .map(move |id| (format!("{area:?}"), id.trim_start_matches("keys#")))
        })
        .chain([("Help".to_owned(), HELP)])
        .collect();
    for (area, section) in sections {
        let prefix = format!("key:{section}:");
        let guide: BTreeSet<String> = index
            .entries()
            .iter()
            .filter(|entry| entry.kind == Kind::Key && entry.id.starts_with(&prefix))
            .flat_map(|entry| chords(&entry.id[prefix.len()..]))
            .collect();
        let fixture: BTreeSet<String> = FIXTURE
            .iter()
            .filter(|chord| chord.section == section)
            .map(|chord| chord.label.to_owned())
            .collect();
        assert!(!guide.is_empty(), "{area} {section}");
        assert_eq!(
            fixture,
            guide,
            "{area} {section}: only in fixture {:?}; only in guide {:?}",
            fixture.difference(&guide).collect::<Vec<_>>(),
            guide.difference(&fixture).collect::<Vec<_>>()
        );
    }
}

#[test]
fn hu4_chord_splitting_follows_the_documented_forms() {
    assert_eq!(chords("Ctrl+Q or Ctrl+C"), ["Ctrl+Q", "Ctrl+C"]);
    assert_eq!(chords("Left, Right or Tab"), ["Left", "Right", "Tab"]);
    assert_eq!(
        chords("Up, Shift+Up or Escape"),
        ["Up", "Shift+Up", "Escape"]
    );
    assert_eq!(chords("Enter"), ["Enter"]);
}

fn overlay_name(overlay: &Option<Overlay>) -> &'static str {
    match overlay {
        None => "none",
        Some(Overlay::Commands(..)) => "commands",
        Some(Overlay::Help(_)) => "help",
        Some(Overlay::HelpEntry(..)) => "help-entry",
        Some(Overlay::ProjectOffer(..)) => "offer",
        Some(Overlay::Config(..)) => "config",
        Some(Overlay::Models(..)) => "models",
        Some(Overlay::Tools(_)) => "tools",
        Some(Overlay::Audit(..)) => "audit",
        Some(Overlay::Exit(_)) => "exit",
        Some(Overlay::Paste(_)) => "paste",
    }
}
fn picker_name(app: &App) -> String {
    match &app.picker {
        None => String::new(),
        Some(Picker::Projects { selected, rows }) => format!("projects {selected}/{}", rows.len()),
        Some(Picker::Models { selected, .. }) => format!("models {selected}"),
    }
}
/// Everything a router decision can change, for the "key was handled" check.
fn snapshot(app: &App) -> String {
    format!(
        "{:?}|{:?}|{:?}|{}|{}|{}|{}|{}|{}|{:?}|{}|{:?}|{}|{:?}|{}|{}|{}|{}|{}|{:?}",
        app.editor.text(),
        app.editor.selection_points(),
        app.focus,
        overlay_name(&app.overlay),
        app.history.is_browsing(),
        app.notice,
        app.capture.is_some(),
        app.history_focus,
        app.history_scroll,
        app.response_selection,
        app.response_scroll,
        app.activity
            .iter()
            .map(|a| a.expanded())
            .collect::<Vec<_>>(),
        picker_name(app),
        app.queue_focused_id,
        app.queue_busy,
        app.models_request,
        app.local_outbox.len(),
        app.help.is_some(),
        app.config_request.is_some(),
        selected_id(app),
    )
}

fn projects(count: u8) -> Vec<asura_control::pb::ProjectReply> {
    (1..=count)
        .map(|id| asura_control::pb::ProjectReply {
            project_id: Some(vec![id; 16]),
            location: Some(format!("/projects/p{id}")),
            current: Some(true),
            ..Default::default()
        })
        .collect()
}
fn models() -> crate::tui::models::Outcome {
    use asura_control::pb::{ModelInventoryEntry, ModelsReply};
    Ok(ModelsReply {
        models: ["system", "coreai:a", "ollama:b"]
            .into_iter()
            .map(|selector| ModelInventoryEntry {
                selector: Some(selector.into()),
                status: Some(1),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    })
}

fn typed(text: &str) -> App {
    let mut app = App::new();
    type_text(&mut app, text);
    app
}
fn app_for(setup: Setup) -> App {
    let mut app = match setup {
        Empty => App::new(),
        Draft | WithLog | WithQueue | StatusProject | StatusModel | ProjectList | ModelList => {
            let mut app = typed("alpha beta gamma");
            for _ in 0..5 {
                press(&mut app, KeyCode::Left);
            }
            app
        }
        Recall | Recalling => {
            let mut app = App::new();
            app.history.record("older");
            type_text(&mut app, "draft");
            if matches!(setup, Recalling) {
                press(&mut app, KeyCode::Up);
                assert!(app.history.is_browsing());
            }
            app
        }
        Undone => {
            let mut app = typed("abc");
            press_with(&mut app, KeyCode::Char('z'), CTRL);
            app
        }
        HelpQuery | HelpStart => {
            let mut app = typed("?que");
            if matches!(setup, HelpQuery) {
                press(&mut app, KeyCode::Down);
                press(&mut app, KeyCode::Down);
            } else {
                press(&mut app, KeyCode::Home);
            }
            app
        }
        LogFocus | Browser => App::new(),
        QueueMiddle | QueueFirst | QueueLast | QueueRejected => App::new(),
    };
    match setup {
        WithLog => with_log(&mut app),
        WithQueue => app.queue_entries = vec![queued(1), queued(2)],
        LogFocus => {
            with_log(&mut app);
            frame(&mut app, 80, 20);
            press_with(&mut app, KeyCode::Up, SHIFT);
            assert!(app.history_focus);
            app.history_scroll = app.history_scroll_max / 2;
        }
        Browser => {
            with_log(&mut app);
            app.transcript[3].1 = (0..80).map(|n| format!("line {n}\n\n")).collect();
            app.rendered_responses[3] = markdown::render(&app.transcript[3].1);
            frame(&mut app, 80, 20);
            press(&mut app, KeyCode::F(6));
            app.response_selection = Some(3);
            frame(&mut app, 80, 20);
            app.response_scroll = 1;
            assert!(app.response_scroll_max > 2);
        }
        QueueMiddle | QueueFirst | QueueLast => {
            app.queue_entries = vec![queued(1), queued(2), queued(3)];
            if matches!(setup, QueueFirst) {
                with_log(&mut app);
            }
            frame(&mut app, 80, 24);
            press_with(&mut app, KeyCode::Up, CTRL);
            assert_eq!(app.focus, Focus::Queue(2));
            if !matches!(setup, QueueLast) {
                press(&mut app, KeyCode::Up);
                if matches!(setup, QueueFirst) {
                    press(&mut app, KeyCode::Home);
                    assert_eq!(app.focus, Focus::Queue(0));
                } else {
                    assert_eq!(app.focus, Focus::Queue(1));
                }
            }
        }
        QueueRejected => {
            app.local_outbox.push(LocalInput {
                request_id: [9; 16],
                project: [1; 16],
                text: "rejected text".into(),
                state: LocalInputState::Rejected,
                steer: None,
            });
            frame(&mut app, 80, 24);
            press_with(&mut app, KeyCode::Up, CTRL);
            assert!(matches!(app.focus, Focus::Queue(_)));
        }
        StatusProject | StatusModel | ProjectList | ModelList => {
            app.projects = Some(projects(3));
            app.selected_project = Some([2; 16]);
            press_with(&mut app, KeyCode::Down, SHIFT);
            assert_eq!(app.focus, Focus::Project);
            match setup {
                StatusModel => {
                    press(&mut app, KeyCode::Right);
                    assert_eq!(app.focus, Focus::Model);
                }
                ProjectList => {
                    press(&mut app, KeyCode::Enter);
                    assert!(matches!(
                        app.picker,
                        Some(Picker::Projects { selected: 1, .. })
                    ));
                }
                ModelList => {
                    app.focus = Focus::Model;
                    app.picker = Some(Picker::Models {
                        selected: 1,
                        result: Some(models()),
                    });
                }
                _ => {}
            }
        }
        _ => {}
    }
    frame(&mut app, 80, 24);
    app
}

#[test]
fn router_handles_every_chord_in_the_fixture() {
    for chord in FIXTURE {
        let mut app = app_for(chord.setup);
        let before = snapshot(&app);
        let exit = press_with(&mut app, chord.code, chord.modifiers);
        assert!(
            exit || snapshot(&app) != before,
            "{} {:?} did nothing in {:?}",
            chord.section,
            chord.label,
            chord.setup
        );
    }
}

#[test]
fn router_ignores_a_chord_the_fixture_does_not_name() {
    // The check above can fail: a key that no router handles changes nothing.
    let mut app = app_for(QueueMiddle);
    let before = snapshot(&app);
    press(&mut app, KeyCode::Char('x'));
    assert_eq!(snapshot(&app), before);
}
