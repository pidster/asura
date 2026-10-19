use super::*;
use rat_text::TextPosition;
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

mod keys;

pub(super) fn press_with(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    app.handle(Event::Key(KeyEvent::new(code, modifiers)))
}
pub(super) fn press(app: &mut App, code: KeyCode) -> bool {
    press_with(app, code, KeyModifiers::NONE)
}
fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}
fn paste(app: &mut App, text: &str) {
    app.handle(Event::Paste(text.into()));
}
pub(super) fn frame(app: &mut App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    terminal.backend().buffer().clone()
}
fn rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}
fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
    rows(&frame(app, width, height))
}
fn text_of(app: &mut App, width: u16, height: u16) -> String {
    screen(app, width, height).join("\n")
}
fn queued(id: u8) -> asura_control::pb::ConversationQueueEntry {
    asura_control::pb::ConversationQueueEntry {
        input_id: Some(vec![id; 16]),
        state: Some(1),
        text: Some(format!("queued {id}")),
        ..Default::default()
    }
}
pub(super) fn with_log(app: &mut App) {
    for index in 0..8 {
        app.append_response(format!("prompt {index}"), None);
        app.transcript[index].1 = format!("answer {index} with enough text to fill a row");
        app.rendered_responses[index] = markdown::render(&app.transcript[index].1);
    }
}
fn selected_id(app: &App) -> Option<String> {
    let help = app.help.as_ref()?;
    let index = help::index()?;
    match help.rows.get(help.selected)? {
        Row::Entry(i) => Some(index.entry(*i)?.id.clone()),
        Row::Area(area) => Some(format!("area:{}", area.name())),
        Row::Guide(id) => Some((*id).to_owned()),
    }
}
fn query(app: &App) -> String {
    app.help.as_ref().unwrap().query.folded().to_owned()
}
/// No worker request, cancellation or retained identity was created.
fn assert_local_only(app: &App) {
    assert!(app.queue_request.is_none() && !app.queue_busy && app.queue_retained.is_none());
    assert!(app.conversation_request.is_none() && app.retained_request.is_none());
    assert!(app.config_request.is_none() && app.project_admin_request.is_none());
    assert!(app.audit_request.is_none() && !app.models_request && !app.models_cancel);
    assert!(!app.cancel_request && !app.audit_cancel);
    assert!(app.local_outbox.is_empty());
}

#[test]
fn hu2_command_rows_and_local_commands_match_in_both_directions() {
    let index = help::index().unwrap();
    let rows: Vec<_> = index
        .entries()
        .iter()
        .filter(|entry| entry.kind == Kind::Command)
        .map(|entry| entry.title.split(' ').next().unwrap().to_owned())
        .collect();
    for name in &rows {
        assert!(
            LOCAL_COMMANDS.contains(&name.as_str()),
            "{name} has no handler"
        );
    }
    for name in LOCAL_COMMANDS {
        assert!(
            rows.iter().any(|row| row == name),
            "{name} has no command row"
        );
    }
}

#[test]
fn hu6_question_mark_at_start_of_an_empty_box_enters_help_mode() {
    for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
        let mut app = App::new();
        press_with(&mut app, KeyCode::Char('?'), modifiers);
        assert!(app.help.is_some(), "{modifiers:?}");
        assert_eq!(app.draft(), "", "the switch is not text");
        assert!(!app.history.is_browsing());
        assert_eq!(app.help.as_ref().unwrap().area, Area::Box);
        // Blank query: the area rows.
        assert_eq!(selected_id(&app).as_deref(), Some("area:Box"));
        assert_local_only(&app);
    }
    // Entering ends recall browsing.
    let mut app = App::new();
    app.history.record("older");
    press(&mut app, KeyCode::Up);
    assert!(app.history.is_browsing());
    press_with(&mut app, KeyCode::Home, KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('?'));
    assert!(app.help.is_some() && !app.history.is_browsing());
    assert_eq!(query(&app), "older");
}

#[test]
fn hu7_existing_text_becomes_the_query() {
    let mut app = App::new();
    type_text(&mut app, "abc");
    press(&mut app, KeyCode::Home);
    press(&mut app, KeyCode::Char('?'));
    assert!(app.help.is_some());
    assert_eq!(app.draft(), "abc");
    assert_eq!(query(&app), "abc");
}

#[test]
fn hu8_question_mark_elsewhere_is_ordinary_text() {
    let mut app = App::new();
    type_text(&mut app, "abc");
    press(&mut app, KeyCode::Home);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Char('?'));
    assert!(app.help.is_none());
    assert_eq!(app.draft(), "a?bc");
    // A selection at byte 0 is replaced; no mode starts.
    let mut app = App::new();
    type_text(&mut app, "abc");
    press_with(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('?'));
    assert!(app.help.is_none());
    assert_eq!(app.draft(), "?");
    // HA3: `a?` and ` ?`.
    for text in ["a?", " ?"] {
        let mut app = App::new();
        type_text(&mut app, text);
        assert!(app.help.is_none(), "{text:?}");
        assert_eq!(app.draft(), text);
    }
    // The first row only: byte 0 is the start of the draft, not of a later row.
    let mut app = App::new();
    type_text(&mut app, "abc");
    press_with(&mut app, KeyCode::Enter, KeyModifiers::ALT);
    press(&mut app, KeyCode::Char('?'));
    assert!(app.help.is_none());
    assert_eq!(app.draft(), "abc\n?");
}

#[test]
fn hu9_entry_guards_block_help_mode() {
    let blocked = |app: &mut App, before: &str| {
        press(app, KeyCode::Char('?'));
        assert!(app.help.is_none());
        assert_eq!(app.draft(), before, "no text and no mode");
    };
    // Overlay.
    let mut app = App::new();
    press(&mut app, KeyCode::F(1));
    blocked(&mut app, "");
    assert!(matches!(app.overlay, Some(Overlay::Help(_))));
    // Paste capture keeps the character as captured text.
    let mut app = App::new();
    press_with(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
    blocked(&mut app, "");
    assert_eq!(app.capture.as_ref().unwrap().text, "?");
    // Picker.
    let mut app = App::new();
    app.projects = Some(vec![asura_control::pb::ProjectReply {
        project_id: Some(vec![1; 16]),
        current: Some(true),
        ..Default::default()
    }]);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    assert!(app.picker.is_some());
    blocked(&mut app, "");
    // Status bar focus, queue focus, log focus and the response browser.
    let mut app = App::new();
    press(&mut app, KeyCode::Down);
    assert_eq!(app.focus, Focus::Project);
    blocked(&mut app, "");
    let mut app = App::new();
    app.queue_entries = vec![queued(1)];
    press_with(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
    assert!(matches!(app.focus, Focus::Queue(_)));
    blocked(&mut app, "");
    let mut app = App::new();
    with_log(&mut app);
    press_with(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
    assert!(app.history_focus);
    blocked(&mut app, "");
    let mut app = App::new();
    with_log(&mut app);
    press(&mut app, KeyCode::F(6));
    assert!(app.response_selection.is_some());
    blocked(&mut app, "");
    // Help mode itself: a second `?` is query text.
    let mut app = App::new();
    press(&mut app, KeyCode::Char('?'));
    press(&mut app, KeyCode::Char('?'));
    assert!(app.help.is_some());
    assert_eq!(app.draft(), "?");
    // A paste beginning with `?` is blocked by the same guards.
    let mut app = App::new();
    press(&mut app, KeyCode::F(1));
    paste(&mut app, "?abc");
    assert!(app.help.is_none() && app.draft().is_empty());
}

#[test]
fn hu10_paste_prefix_enters_help_mode_only_at_byte_zero() {
    let mut app = App::new();
    paste(&mut app, "?abc");
    assert!(app.help.is_some());
    assert_eq!(app.draft(), "abc");
    assert_eq!(query(&app), "abc");
    let mut app = App::new();
    paste(&mut app, "?");
    assert!(app.help.is_some());
    assert_eq!(app.draft(), "");
    assert!(app.help.as_ref().unwrap().query.is_blank());
    let mut app = App::new();
    paste(&mut app, "a?");
    assert!(app.help.is_none());
    assert_eq!(app.draft(), "a?");
    // With the cursor past byte 0 a leading `?` is ordinary text.
    let mut app = App::new();
    type_text(&mut app, "x");
    paste(&mut app, "?abc");
    assert!(app.help.is_none());
    assert_eq!(app.draft(), "x?abc");
    // In help mode a paste adds to the query.
    let mut app = App::new();
    paste(&mut app, "?que");
    paste(&mut app, "ue");
    assert_eq!(query(&app), "queue");
    assert!(!app.history.is_browsing());
}

#[test]
fn hu11_backspace_at_byte_zero_leaves_help_mode_and_keeps_the_text() {
    let mut app = App::new();
    type_text(&mut app, "abc");
    press(&mut app, KeyCode::Home);
    press(&mut app, KeyCode::Char('?'));
    assert!(app.help.is_some());
    press(&mut app, KeyCode::Backspace);
    assert!(app.help.is_none());
    assert_eq!(app.draft(), "abc");
    let (anchor, cursor) = app.editor.selection_points();
    assert_eq!(
        (anchor, cursor),
        (TextPosition::new(0, 0), TextPosition::new(0, 0))
    );
    // A later Backspace at byte 0 does nothing, as before.
    press(&mut app, KeyCode::Backspace);
    assert_eq!(app.draft(), "abc");
    // Backspace after byte 0 edits the query; with a selection it deletes.
    press(&mut app, KeyCode::Char('?'));
    press(&mut app, KeyCode::End);
    press(&mut app, KeyCode::Backspace);
    assert!(app.help.is_some());
    assert_eq!(app.draft(), "ab");
}

#[test]
fn hu12_escape_leaves_while_tab_and_alt_enter_change_nothing() {
    let mut app = App::new();
    type_text(&mut app, "?que");
    let before = (
        app.draft(),
        app.editor.selection_points(),
        selected_id(&app),
    );
    press(&mut app, KeyCode::Tab);
    press_with(&mut app, KeyCode::Enter, KeyModifiers::ALT);
    assert!(app.help.is_some());
    assert_eq!(
        (
            app.draft(),
            app.editor.selection_points(),
            selected_id(&app)
        ),
        before
    );
    assert!(app.overlay.is_none() && app.picker.is_none());
    press(&mut app, KeyCode::Esc);
    assert!(app.help.is_none());
    assert_eq!(app.draft(), "que");
    // Escape on an empty query also leaves.
    let mut app = App::new();
    press(&mut app, KeyCode::Char('?'));
    press(&mut app, KeyCode::Esc);
    assert!(app.help.is_none());
    // With an opened entry, Escape closes only the entry.
    let mut app = App::new();
    type_text(&mut app, "?que");
    press(&mut app, KeyCode::Enter);
    assert!(matches!(app.overlay, Some(Overlay::HelpEntry(..))));
    press(&mut app, KeyCode::Esc);
    assert!(app.overlay.is_none() && app.help.is_some());
    assert_eq!(app.draft(), "que");
}

#[test]
fn hu13_selection_moves_clamps_and_pages_without_recall_or_focus_change() {
    let mut app = App::new();
    app.history.record("older");
    type_text(&mut app, "?up");
    screen(&mut app, 80, 24);
    let help = app.help.as_ref().unwrap();
    assert!(help.rows.len() >= 10, "{}", help.rows.len());
    let count = help.rows.len();
    let page = help.page;
    assert!(page > 1);
    press(&mut app, KeyCode::Up);
    assert_eq!(app.help.as_ref().unwrap().selected, 0);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.help.as_ref().unwrap().selected, 2);
    press(&mut app, KeyCode::PageDown);
    assert_eq!(app.help.as_ref().unwrap().selected, 2 + page);
    press(&mut app, KeyCode::PageUp);
    assert_eq!(app.help.as_ref().unwrap().selected, 2);
    for _ in 0..count + 5 {
        press(&mut app, KeyCode::Down);
    }
    assert_eq!(app.help.as_ref().unwrap().selected, count - 1);
    press(&mut app, KeyCode::PageDown);
    assert_eq!(app.help.as_ref().unwrap().selected, count - 1);
    for _ in 0..count + 5 {
        press(&mut app, KeyCode::PageUp);
    }
    assert_eq!(app.help.as_ref().unwrap().selected, 0);
    assert!(!app.history.is_browsing());
    assert_eq!(app.focus, Focus::Editor);
    assert_eq!(app.draft(), "up");
    // The selected row stays visible when the list scrolls.
    for _ in 0..9 {
        press(&mut app, KeyCode::Down);
    }
    let rows = text_of(&mut app, 80, 24);
    let help = app.help.as_ref().unwrap();
    assert!(help.first > 0 && help.selected < help.first + help.page);
    assert!(rows.contains(&format!("{}/{count}", help.selected + 1)));
}

#[test]
fn hu14_recall_never_runs_in_help_mode() {
    let mut app = App::new();
    app.history.record("older");
    type_text(&mut app, "?queue");
    let before = (app.draft(), selected_id(&app));
    press(&mut app, KeyCode::Up);
    press_with(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
    press_with(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    assert!(!app.history.is_browsing());
    assert_eq!((app.draft(), selected_id(&app)), before);
    // Leaving makes the text an ordinary draft; recall still sees only "older".
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Up);
    assert_eq!(app.draft(), "older");
}

#[test]
fn hu15_enter_opens_the_entry_without_submitting_or_recording() {
    let mut app = App::new();
    app.history.record("real input");
    type_text(&mut app, "?queue");
    press(&mut app, KeyCode::Enter);
    assert!(matches!(app.overlay, Some(Overlay::HelpEntry(..))));
    assert_local_only(&app);
    assert!(app.notice.is_empty());
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.draft(), "queue");
    press(&mut app, KeyCode::Esc);
    // A cleared box and Up: the previous real input, never a help query.
    press_with(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Up);
    assert_eq!(app.draft(), "real input");
    press(&mut app, KeyCode::Up);
    assert_eq!(app.draft(), "real input", "no older entry exists");
    // No matches: a notice, the text is kept, nothing opens.
    let mut app = App::new();
    type_text(&mut app, "?zzzzqq");
    assert!(text_of(&mut app, 80, 24).contains("No matches"));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.notice, "No help matches");
    assert!(app.overlay.is_none());
    assert_eq!(app.draft(), "zzzzqq");
}

/// CF06b: `/project list after ID` is a help command row; searching for it
/// selects and opens it without submitting a project list request.
#[test]
fn project_list_after_is_found_and_opened_locally() {
    let index = help::index().unwrap();
    assert!(index.find("command:/project list after ID").is_some());
    let mut app = App::new();
    type_text(&mut app, "?list after");
    assert_eq!(
        selected_id(&app).as_deref(),
        Some("command:/project list after ID")
    );
    press(&mut app, KeyCode::Enter);
    assert!(matches!(app.overlay, Some(Overlay::HelpEntry(..))));
    let text = text_of(&mut app, 100, 30);
    assert!(text.contains("/project list after ID"), "{text}");
    assert_local_only(&app);
}

#[test]
fn hu16_focus_moving_keys_are_ignored() {
    let mut app = App::new();
    with_log(&mut app);
    app.queue_entries = vec![queued(1)];
    type_text(&mut app, "?up");
    let before = (selected_id(&app), app.draft());
    for (code, modifiers) in [
        (KeyCode::Up, KeyModifiers::SHIFT),
        (KeyCode::Down, KeyModifiers::SHIFT),
        (KeyCode::Up, KeyModifiers::CONTROL),
        (KeyCode::Down, KeyModifiers::CONTROL),
        (KeyCode::F(6), KeyModifiers::NONE),
    ] {
        press_with(&mut app, code, modifiers);
        assert_eq!(app.focus, Focus::Editor, "{code:?}");
        assert!(!app.history_focus && app.response_selection.is_none());
        assert!(app.help.is_some() && app.overlay.is_none());
        assert_eq!((selected_id(&app), app.draft()), before);
    }
}

#[test]
fn hu17_steer_queue_and_exit_keys_are_blocked_with_a_notice() {
    for key in ['s', 't', 'd'] {
        let mut app = App::new();
        type_text(&mut app, "?abc");
        let exit = press_with(&mut app, KeyCode::Char(key), KeyModifiers::CONTROL);
        assert!(!exit);
        assert_eq!(app.notice, "Leave help mode first", "{key}");
        assert!(app.help.is_some() && app.overlay.is_none());
        assert_eq!(app.draft(), "abc");
        assert_local_only(&app);
    }
    // Ctrl+D on an empty query is blocked too; Ctrl+Q and Ctrl+C keep their
    // behavior, and a nonempty query counts as a draft.
    let mut app = App::new();
    press(&mut app, KeyCode::Char('?'));
    assert!(!press_with(
        &mut app,
        KeyCode::Char('d'),
        KeyModifiers::CONTROL
    ));
    assert!(app.help.is_some());
    for key in ['q', 'c'] {
        let mut app = App::new();
        type_text(&mut app, "?abc");
        assert!(!press_with(
            &mut app,
            KeyCode::Char(key),
            KeyModifiers::CONTROL
        ));
        assert!(matches!(app.overlay, Some(Overlay::Exit(false))));
        let mut empty = App::new();
        press(&mut empty, KeyCode::Char('?'));
        assert!(press_with(
            &mut empty,
            KeyCode::Char(key),
            KeyModifiers::CONTROL
        ));
    }
}

#[test]
fn hu19_long_query_is_cut_and_never_panics() {
    let mut app = App::new();
    let words: Vec<String> = (0..12).map(|n| format!("queue{n}")).collect();
    let mut text = words.join(" ");
    text.push_str(&"é".repeat(150 - text.len() / 2));
    assert!(text.len() > 256);
    paste(&mut app, &format!("?{text}"));
    assert!(app.help.is_some());
    assert_eq!(
        app.help.as_ref().unwrap().query.terms().len(),
        help::MAX_TERMS
    );
    screen(&mut app, 80, 24);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.notice, "No help matches");
    assert_eq!(app.draft().len(), text.len(), "the text is kept whole");
}

#[test]
fn hu20_last_area_follows_the_tracking_table() {
    let mut app = App::new();
    with_log(&mut app);
    app.queue_entries = vec![queued(1)];
    app.projects = Some(vec![asura_control::pb::ProjectReply {
        project_id: Some(vec![1; 16]),
        current: Some(true),
        ..Default::default()
    }]);
    screen(&mut app, 80, 24);
    assert_eq!(app.last_area, Area::Box);
    press_with(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
    assert_eq!(app.last_area, Area::Queue);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.last_area, Area::Queue, "returning to the box keeps it");
    press_with(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
    press_with(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
    assert!(app.history_focus);
    assert_eq!(app.last_area, Area::Log);
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::F(6));
    assert_eq!(app.last_area, Area::Log, "the response browser is the log");
    press(&mut app, KeyCode::Esc);
    press_with(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
    assert_eq!(app.focus, Focus::Project);
    assert_eq!(app.last_area, Area::StatusBar);
    press(&mut app, KeyCode::Enter);
    assert!(app.picker.is_some());
    assert_eq!(app.last_area, Area::StatusBar);
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.focus, Focus::Editor);
    assert_eq!(app.last_area, Area::StatusBar);
    // Submitting from the box resets it, whether or not the submit succeeds.
    type_text(&mut app, "hello");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.last_area, Area::Box);
    // Help mode captures the area on entry and never moves it.
    press_with(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.last_area, Area::Queue);
    press_with(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Char('?'));
    assert_eq!(app.help.as_ref().unwrap().area, Area::Queue);
    press_with(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
    assert_eq!(app.last_area, Area::Queue);
}

#[test]
fn hu21_blank_query_enter_opens_the_last_used_areas_help() {
    let expected = [
        (Area::Box, "Input box keys"),
        (Area::Queue, "Queue keys"),
        (Area::Log, "Conversation log keys"),
        (Area::StatusBar, "Status bar keys"),
    ];
    for (area, section) in expected {
        let mut app = App::new();
        with_log(&mut app);
        app.queue_entries = vec![queued(1)];
        app.projects = Some(vec![asura_control::pb::ProjectReply {
            project_id: Some(vec![1; 16]),
            current: Some(true),
            ..Default::default()
        }]);
        screen(&mut app, 80, 24);
        match area {
            Area::Box => {}
            Area::Queue => {
                press_with(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
                press(&mut app, KeyCode::Esc);
            }
            Area::Log => {
                press(&mut app, KeyCode::F(6));
                press(&mut app, KeyCode::Esc);
            }
            Area::StatusBar => {
                press_with(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
                press(&mut app, KeyCode::Esc);
            }
        }
        assert_eq!(app.last_area, area);
        press(&mut app, KeyCode::Char('?'));
        let panel = text_of(&mut app, 80, 24);
        assert!(
            panel.contains(&format!("{} (last used)", area.name())),
            "{panel}"
        );
        assert!(panel.contains("Area help"));
        assert_eq!(selected_id(&app), Some(format!("area:{}", area.name())));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.overlay, Some(Overlay::HelpEntry(..))));
        let opened = text_of(&mut app, 100, 50);
        assert!(
            opened.contains(&format!("Help · {}", area.name())),
            "{opened}"
        );
        assert!(opened.contains(section), "{area:?}\n{opened}");
        assert_local_only(&app);
    }
    // Other area rows and the guide rows are reachable and open.
    let mut app = App::new();
    press(&mut app, KeyCode::Char('?'));
    let ids: Vec<_> = (0..app.help.as_ref().unwrap().rows.len())
        .map(|n| {
            app.help.as_mut().unwrap().selected = n;
            selected_id(&app).unwrap()
        })
        .collect();
    assert_eq!(
        ids,
        [
            "area:Box",
            "area:Queue",
            "area:Log",
            "area:Status bar",
            "commands#commands-reference",
            "keys#keys-reference"
        ]
    );
    for n in 0..ids.len() {
        app.help.as_mut().unwrap().selected = n;
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.overlay, Some(Overlay::HelpEntry(..))), "{n}");
        press(&mut app, KeyCode::Esc);
    }
}

#[test]
fn results_follow_edits_and_selection_resets_only_when_the_query_changes() {
    let mut app = App::new();
    type_text(&mut app, "?queue");
    let top = selected_id(&app);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    let second = selected_id(&app);
    assert_ne!(top, second);
    // Cursor keys and a trailing space leave the folded query, and selection, alone.
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::End);
    type_text(&mut app, " ");
    assert_eq!(selected_id(&app), second);
    // A new term narrows the list and selects the first row.
    type_text(&mut app, "enter");
    assert_eq!(app.help.as_ref().unwrap().selected, 0);
    assert_eq!(query(&app), "queue enter");
    // Emptying the query shows the area rows again.
    press_with(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Delete);
    assert!(app.help.as_ref().unwrap().query.is_blank());
    assert_eq!(selected_id(&app).as_deref(), Some("area:Box"));
}

#[test]
fn he_help_mode_survives_unavailable_backend_state_and_service_results() {
    let mut app = App::new();
    type_text(&mut app, "?config");
    assert!(app.help.is_some());
    // A service result arriving in help mode opens above the panel; Escape
    // closes it and help mode continues.
    app.config_result = Some("model: system".into());
    let shown = text_of(&mut app, 80, 24);
    assert!(matches!(app.overlay, Some(Overlay::Config(..))));
    assert!(shown.contains("model: system") && shown.contains("? Help"));
    press(&mut app, KeyCode::Esc);
    assert!(app.overlay.is_none() && app.help.is_some());
    assert_eq!(app.draft(), "config");
    // F1 opens the /help overlay; help mode resumes when it closes.
    press(&mut app, KeyCode::F(1));
    assert!(matches!(app.overlay, Some(Overlay::Help(_))));
    press(&mut app, KeyCode::F(1));
    assert!(app.overlay.is_none() && app.help.is_some());
    assert_local_only(&app);
}

fn input_row(buffer: &Buffer) -> u16 {
    // The marker cell is the only bold glow `?` or `›` at the box's left edge.
    (0..buffer.area.height)
        .rev()
        .find(|&y| {
            let cell = &buffer[(1, y)];
            matches!(cell.symbol(), "?" | "›") && cell.fg == INPUT_GLOW
        })
        .expect("input marker")
}

#[test]
fn hr1_box_and_strips_use_help_indigo_with_glow_marker_and_bold_text() {
    let mut app = App::new();
    type_text(&mut app, "?proj");
    let buffer = frame(&mut app, 80, 24);
    let y = input_row(&buffer);
    assert_eq!(y, 20);
    let marker = &buffer[(1, y)];
    assert_eq!(marker.symbol(), "?");
    assert_eq!(marker.fg, INPUT_GLOW);
    assert!(marker.modifier.contains(Modifier::BOLD));
    assert_eq!(marker.bg, HELP_INDIGO);
    for x in 0..80 {
        assert_eq!(buffer[(x, y)].bg, HELP_INDIGO, "box x={x}");
    }
    for (offset, x) in (3..7).enumerate() {
        let cell = &buffer[(x, y)];
        assert_eq!(cell.symbol(), &"proj"[offset..=offset]);
        assert_eq!((cell.fg, cell.bg), (INPUT_TEXT, HELP_INDIGO));
        assert!(cell.modifier.contains(Modifier::BOLD));
    }
    for strip in [y - 1, y + 1] {
        for x in 0..80 {
            assert_eq!(buffer[(x, strip)].fg, HELP_INDIGO, "strip {strip} x={x}");
        }
    }
    // Normal mode keeps the standard tint and marker; leaving restores both.
    press(&mut app, KeyCode::Home);
    press(&mut app, KeyCode::Backspace);
    let buffer = frame(&mut app, 80, 24);
    let y = input_row(&buffer);
    assert_eq!(buffer[(1, y)].symbol(), "›");
    assert_eq!(buffer[(1, y)].bg, INPUT_TINT);
    assert_eq!(buffer[(0, y + 1)].fg, INPUT_TINT);
    // Command highlighting does not apply to a query.
    let mut app = App::new();
    type_text(&mut app, "?/help");
    let buffer = frame(&mut app, 80, 24);
    let y = input_row(&buffer);
    for x in 3..8 {
        assert_eq!(buffer[(x, y)].fg, INPUT_TEXT, "x={x}");
    }
}

#[test]
fn hr2_scrolled_multi_row_query_shows_no_marker() {
    let mut app = App::new();
    paste(&mut app, &format!("?{}", "line\n".repeat(12)));
    let buffer = frame(&mut app, 80, 24);
    let markers = (0..buffer.area.height)
        .filter(|&y| {
            matches!(buffer[(1, y)].symbol(), "?" | "›") && buffer[(1, y)].fg == INPUT_GLOW
        })
        .count();
    assert_eq!(markers, 0);
    let mut short = App::new();
    paste(&mut short, "?one\ntwo");
    let buffer = frame(&mut short, 80, 24);
    let markers = (0..buffer.area.height)
        .filter(|&y| buffer[(1, y)].symbol() == "?" && buffer[(1, y)].fg == INPUT_GLOW)
        .count();
    assert_eq!(markers, 1);
}

fn kind_rows(lines: &[String]) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            ["Key ", "Command ", "Topic ", "Planned "]
                .iter()
                .any(|label| {
                    line.trim_start()
                        .trim_start_matches('›')
                        .trim_start()
                        .starts_with(label)
                })
        })
        .map(|(row, _)| row)
        .collect()
}

#[test]
fn hr3_small_terminal_shows_header_four_rows_and_padding() {
    // The box has strips from an inner width of 60 and a height of 16, so the
    // pinned four rows appear at 62x16.
    let mut app = App::new();
    type_text(&mut app, "?queue");
    let lines = screen(&mut app, 62, 16);
    let header = lines.iter().position(|l| l.contains("? Help")).unwrap();
    let results = kind_rows(&lines);
    assert_eq!(
        results,
        [header + 1, header + 2, header + 3, header + 4],
        "{lines:#?}"
    );
    assert!(lines[header - 1].trim().is_empty(), "top padding");
    assert!(lines[header + 5].trim().is_empty(), "bottom padding");
    assert!(lines[header].contains("results") && lines[header].contains("1/"));
    // The selected row stays visible while scrolling down.
    for _ in 0..6 {
        press(&mut app, KeyCode::Down);
    }
    let lines = screen(&mut app, 62, 16);
    assert_eq!(kind_rows(&lines).len(), 4);
    assert!(lines.iter().any(|l| l.contains("7/")));
    assert!(lines.iter().any(|l| l.contains('›')));
    // Without strips (60x16) the panel gains the strip rows.
    let mut app = App::new();
    type_text(&mut app, "?queue");
    let lines = screen(&mut app, 60, 16);
    assert_eq!(kind_rows(&lines).len(), 6, "{lines:#?}");
}

#[test]
fn hr4_narrow_widths_omit_summaries_and_strips_follow_thresholds() {
    let strip = |buffer: &Buffer, y: u16| {
        (0..buffer.area.width).all(|x| matches!(buffer[(x, y)].symbol(), "▀" | "▄" | "█"))
    };
    // Summaries and strips both need an inner width of 60.
    let mut wide = App::new();
    type_text(&mut wide, "?queue");
    let buffer = frame(&mut wide, 62, 16);
    assert!(rows(&buffer).iter().any(|l| l.contains("Select a message")));
    let input = input_row(&buffer);
    assert!(strip(&buffer, input - 1) && strip(&buffer, input + 1));
    for (width, height) in [(59u16, 16u16), (60, 15), (60, 16), (62, 15)] {
        let mut app = App::new();
        type_text(&mut app, "?queue");
        let buffer = frame(&mut app, width, height);
        let lines = rows(&buffer);
        let has_summary = lines.iter().any(|l| l.contains("Select a message"));
        assert_eq!(has_summary, width >= 62, "{width}x{height}");
        assert!(
            !kind_rows(&lines).is_empty(),
            "{width}x{height}: {lines:#?}"
        );
        let input = input_row(&buffer);
        assert!(
            !strip(&buffer, input - 1) && !strip(&buffer, input + 1),
            "{width}x{height}"
        );
        assert!(lines[usize::from(input)].contains("queue"));
    }
}

#[test]
fn hr5_minimum_size_hides_the_panel_and_hints_the_selected_title() {
    let mut app = App::new();
    type_text(&mut app, "?queue");
    let lines = screen(&mut app, 30, 8);
    assert!(!lines.iter().any(|l| l.contains("? Help")));
    assert!(kind_rows(&lines).is_empty());
    assert!(
        lines.last().unwrap().contains("? Queue: Up or Down"),
        "{lines:#?}"
    );
    assert!(lines.last().unwrap().contains("↵ open"));
    // Selection still moves, and Enter still opens, with the panel hidden.
    press(&mut app, KeyCode::Down);
    let lines = screen(&mut app, 30, 8);
    assert!(lines.last().unwrap().contains("Queue: Home or End"));
    // No results: the hint says so.
    let mut none = App::new();
    type_text(&mut none, "?zzzzqq");
    let lines = screen(&mut none, 30, 8);
    assert!(lines.last().unwrap().contains("? No help matches"));
}

#[test]
fn hr6_queue_panel_is_hidden_and_the_header_counts_queued_rows() {
    let mut app = App::new();
    app.queue_entries = vec![queued(1), queued(2)];
    let normal = text_of(&mut app, 80, 24);
    assert!(normal.contains("Queued messages"));
    type_text(&mut app, "?queue");
    let lines = screen(&mut app, 80, 24);
    assert!(!lines.join("\n").contains("Queued messages"));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("? Help") && l.contains("2 queued"))
    );
    press(&mut app, KeyCode::Esc);
    assert!(text_of(&mut app, 80, 24).contains("Queued messages"));
    // A queue with nothing waiting shows no count.
    let mut idle = App::new();
    type_text(&mut idle, "?queue");
    assert!(!text_of(&mut idle, 80, 24).contains("queued"));
}

#[test]
fn hr7_selected_row_has_marker_and_selection_colours_only() {
    let mut app = App::new();
    type_text(&mut app, "?queue");
    press(&mut app, KeyCode::Down);
    let buffer = frame(&mut app, 80, 24);
    let lines = rows(&buffer);
    let results = kind_rows(&lines);
    assert!(results.len() >= 3);
    for (position, &y) in results.iter().enumerate() {
        let selected = position == 1;
        let marker = buffer[(1, y as u16)].symbol() == "›";
        assert_eq!(marker, selected, "row {position}");
        for x in 1..79 {
            let cell = &buffer[(x, y as u16)];
            assert_eq!(
                cell.bg == Color::Rgb(70, 82, 94),
                selected,
                "row {position} x={x}"
            );
            if selected {
                assert_eq!(cell.fg, Color::Rgb(245, 248, 250), "row {position} x={x}");
            } else {
                assert_ne!(cell.fg, Color::Rgb(245, 248, 250), "row {position} x={x}");
            }
        }
    }
}

#[test]
fn hr8_opened_entry_shows_title_hints_and_scrolling_clamps() {
    let mut app = App::new();
    type_text(&mut app, "?queue");
    press(&mut app, KeyCode::Enter);
    // Short terminal: the entry exceeds the space.
    let lines = screen(&mut app, 80, 16);
    let title = lines.iter().find(|l| l.contains("Help · ")).unwrap();
    assert!(
        title.contains("Esc back") && title.contains("arrows scroll"),
        "{title}"
    );
    let max = app.panel_scroll_max;
    assert!(max > 0);
    for _ in 0..max + 20 {
        press(&mut app, KeyCode::Down);
    }
    let lines = screen(&mut app, 80, 16);
    assert!(matches!(app.overlay, Some(Overlay::HelpEntry(_, _, offset)) if offset == max));
    assert!(
        lines.iter().any(|l| l.contains("Help · ")),
        "title stays fixed"
    );
    for _ in 0..max + 20 {
        press(&mut app, KeyCode::Up);
    }
    screen(&mut app, 80, 16);
    assert!(matches!(app.overlay, Some(Overlay::HelpEntry(_, _, 0))));
    // Tall terminal: everything fits and the scroll hint is absent.
    let lines = screen(&mut app, 80, 80);
    let title = lines.iter().find(|l| l.contains("Help · ")).unwrap();
    assert!(title.contains("Esc back") && !title.contains("arrows scroll"));
    assert_eq!(app.panel_scroll_max, 0);
    // F1 closes the entry as well as Escape, back to the panel and its query.
    press(&mut app, KeyCode::F(1));
    assert!(app.overlay.is_none() && app.help.is_some());
    assert_eq!(app.draft(), "queue");
    assert_eq!(app.help.as_ref().unwrap().selected, 0);
}

#[test]
fn hr9_help_table_rows_equal_the_index_in_guide_order() {
    let index = help::index().unwrap();
    let table = super::super::help_table(400);
    let mut lines = table.lines();
    assert!(lines.next().unwrap().starts_with("Command / key"));
    assert!(lines.next().unwrap().contains('┼'));
    // Titles and actions wrap inside their columns; the cells, read in order,
    // are the index rows' text in guide order.
    let normalize = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let (mut titles, mut actions) = (Vec::new(), Vec::new());
    for line in lines {
        let (left, right) = line.split_once(" │ ").unwrap();
        titles.push(left.trim().to_owned());
        actions.push(right.trim().to_owned());
    }
    let want: Vec<(String, String)> = index
        .table_rows()
        .into_iter()
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .collect();
    assert_eq!(
        normalize(&titles.join(" ")),
        normalize(
            &want
                .iter()
                .map(|r| r.0.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        )
    );
    assert_eq!(
        normalize(&actions.join(" ")),
        normalize(
            &want
                .iter()
                .map(|r| r.1.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        )
    );
    // Commands first, in guide order, then keys.
    let titles: Vec<_> = want.iter().map(|row| row.0.as_str()).collect();
    assert_eq!(titles[0], "/help");
    let first_key = titles.iter().position(|t| t.contains(": ")).unwrap();
    assert!(titles[..first_key].iter().all(|t| t.starts_with('/')));
    assert!(titles[first_key..].iter().all(|t| t.contains(": ")));
    // The overlay shows the first rows at 80x24.
    let mut app = App::new();
    press(&mut app, KeyCode::F(1));
    let shown = text_of(&mut app, 80, 24);
    assert!(shown.contains("/help") && shown.contains("/quit"));
    assert!(shown.contains("arrows scroll"));
}

#[test]
fn hr10_shrinking_through_every_mode_renders_every_frame() {
    let sizes = [
        (120, 40),
        (100, 30),
        (80, 24),
        (70, 18),
        (60, 16),
        (60, 15),
        (59, 16),
        (45, 12),
        (40, 10),
        (30, 9),
        (30, 8),
        (29, 8),
        (30, 7),
        (20, 5),
        (3, 3),
        (1, 1),
    ];
    for setup in 0..5 {
        let mut app = App::new();
        app.queue_entries = vec![queued(1)];
        match setup {
            0 => type_text(&mut app, "?queue"),
            1 => type_text(&mut app, "?"),
            2 => type_text(&mut app, "?zzzzqq"),
            3 => {
                type_text(&mut app, "?queue");
                press(&mut app, KeyCode::Enter);
            }
            _ => {
                paste(&mut app, &format!("?{}", "wrap me\n".repeat(10)));
                press(&mut app, KeyCode::Down);
            }
        }
        for (width, height) in sizes {
            frame(&mut app, width, height);
        }
        for (width, height) in sizes.into_iter().rev() {
            frame(&mut app, width, height);
        }
        assert!(app.help.is_some());
    }
}

#[test]
fn unavailable_index_shows_one_row_and_never_panics() {
    // The defensive path is reached through the help state, not the content.
    let mut app = App::new();
    press(&mut app, KeyCode::Char('?'));
    app.help.as_mut().unwrap().rows.clear();
    app.help.as_mut().unwrap().query = Query::new("zzz");
    let lines = screen(&mut app, 80, 24);
    assert!(lines.iter().any(|l| l.contains("No matches")));
    assert_eq!(help::UNAVAILABLE, "Help content unavailable in this build");
}

#[test]
fn query_search_runs_without_io_or_requests_under_overload() {
    // HA9/AE-07 at unit level: many refreshes with unavailable backend state.
    let mut app = App::new();
    type_text(&mut app, "?");
    for round in 0..200 {
        type_text(&mut app, "queue enter shift up");
        press_with(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Delete);
        assert!(
            app.help.as_ref().unwrap().rows.len() <= 6 + MAX_RESULTS,
            "{round}"
        );
    }
    assert_local_only(&app);
}

#[test]
fn paste_capture_insert_adds_to_the_query() {
    let mut app = App::new();
    type_text(&mut app, "?que");
    press_with(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
    type_text(&mut app, "ue shift");
    press_with(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
    assert!(matches!(app.overlay, Some(Overlay::Paste(None))));
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Enter);
    assert!(app.overlay.is_none() && app.capture.is_none());
    assert_eq!(app.draft(), "queue shift");
    assert_eq!(query(&app), "queue shift");
    assert!(app.help.is_some());
}

#[test]
fn index_build_and_refresh_stay_far_inside_their_budgets() {
    // Release objectives are 10 ms (build, once) and 2 ms (refresh). Debug
    // builds are an order of magnitude slower, so these bounds only catch a
    // change in complexity; the measured values are in the design's notes.
    let started = std::time::Instant::now();
    let index = help::Index::build(&help::embedded()).unwrap();
    let build = started.elapsed();
    assert!(build < std::time::Duration::from_millis(250), "{build:?}");
    let query = Query::new("queue enter shift up input box keys response");
    let started = std::time::Instant::now();
    for _ in 0..100 {
        assert!(index.search(&query).work <= index.entries().len() * help::MAX_TERMS);
    }
    let refresh = started.elapsed() / 100;
    assert!(
        refresh < std::time::Duration::from_millis(20),
        "{refresh:?}"
    );
    println!("help index build {build:?}, refresh {refresh:?}");
}
