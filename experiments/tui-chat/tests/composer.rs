//! TP1/TP2 integration through the actual editor, application and renderer.

use asura_tui_chat::{
    app::App,
    model::Action,
    ui::{self, Palette},
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

fn row(buffer: &Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

#[test]
fn ts2_half_block_edges_replace_adjacent_blank_separators() {
    use asura_tui_chat::model::{TranscriptEntry, TranscriptKind};
    for light in [false, true] {
        for (width, expected) in [
            (
                120,
                vec![
                    "before",
                    "▄",
                    "› first",
                    "▀",
                    "▄",
                    "› second",
                    "▀",
                    "after",
                    "",
                    "last",
                    "",
                ],
            ),
            (
                40,
                vec![
                    "before",
                    "",
                    "› first",
                    "",
                    "› second",
                    "",
                    "after",
                    "",
                    "last",
                    "",
                ],
            ),
        ] {
            let mut app = App::new(true);
            app.fixture.transcript = [
                (TranscriptKind::Output, "before"),
                (TranscriptKind::User(Action::NewTurn), "first"),
                (TranscriptKind::User(Action::Steer), "second"),
                (TranscriptKind::Output, "after"),
                (TranscriptKind::Output, "last"),
            ]
            .into_iter()
            .map(|(kind, text)| TranscriptEntry {
                kind,
                text: text.into(),
            })
            .collect();
            app.viewport.scroll_rows(-100);
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal
                .draw(|frame| ui::draw(frame, &mut app, light))
                .unwrap();
            for (index, expected) in expected.iter().enumerate() {
                let actual = row(terminal.backend().buffer(), index as u16 + 1);
                if matches!(*expected, "▄" | "▀") {
                    assert!(actual.chars().all(|ch| ch.to_string() == *expected));
                } else {
                    let compact_spaces = actual.split_whitespace().collect::<Vec<_>>().join(" ");
                    assert_eq!(compact_spaces, *expected, "width {width}, row {index}");
                }
            }
        }
    }
}

#[test]
fn ts2_user_history_matches_input_spacing_and_has_a_darker_full_width_band() {
    use ratatui::{layout::Rect, style::Color};
    for light in [false, true] {
        let palette = Palette::new(light);
        let (Color::Rgb(r, g, b), Color::Rgb(ir, ig, ib)) = (palette.user_band, palette.band)
        else {
            panic!("both band colors are explicit RGB");
        };
        assert!(r < ir && g < ig && b < ib);
        for (width, height) in [
            (120, 40),
            (80, 24),
            (60, 16),
            (59, 16),
            (60, 15),
            (40, 12),
            (30, 8),
        ] {
            let mut app = App::new(true);
            app.fixture.transcript.clear();
            app.fixture
                .submit("first e\u{301} 👩🏽‍💻\n\n界 third".into(), 0)
                .unwrap();
            app.fixture.acknowledge_pending();
            app.editor.insert("editable draft").unwrap();
            app.viewport.scroll_rows(-100);
            let area = Rect::new(2, 3, width, height);
            let mut terminal = Terminal::new(TestBackend::new(width + 4, height + 6)).unwrap();
            let completed = terminal
                .draw(|frame| ui::draw_in_area(frame, &mut app, light, area, ""))
                .unwrap();
            let geometry = ui::app_geometry(&app, area);
            let top = geometry.transcript.y;
            let strips = u16::from(width >= 60 && height >= 16);
            let first = top + strips;
            // CompletedFrame includes continuation cells that terminal diffing
            // correctly skips when emitting a wide glyph to a real terminal.
            let buffer = completed.buffer;
            assert_eq!(buffer[(area.x + 1, first)].symbol(), "›");
            assert_eq!(buffer[(area.x + 3, first)].symbol(), "f");
            assert!(row(buffer, first).contains("first e\u{301} 👩🏽‍💻"));
            assert!(!row(buffer, first).contains("You"));
            for y in first..(first + 3).min(geometry.transcript.bottom()) {
                for x in area.x..area.right() {
                    assert_eq!(
                        buffer[(x, y)].bg,
                        palette.user_band,
                        "{width}x{height} ({x}, {y})"
                    );
                }
            }
            if first + 2 < geometry.transcript.bottom() {
                assert_eq!(buffer[(area.x + 3, first + 2)].symbol(), "界");
            }
            if strips > 0 {
                assert_eq!(buffer[(area.x, top)].symbol(), "▄");
                assert_eq!(buffer[(area.x, top)].fg, palette.user_band);
                assert_eq!(buffer[(area.x, first + 3)].symbol(), "▀");
                assert_eq!(buffer[(area.x, first + 3)].bg, palette.base);
            }
            let separator = first + 3 + strips;
            if strips == 0 && separator < geometry.transcript.bottom() {
                assert_eq!(buffer[(area.x + 3, separator)].symbol(), " ");
                assert_eq!(buffer[(area.x, separator)].bg, palette.base);
            }
            let output_y = separator + u16::from(strips == 0);
            if output_y < geometry.transcript.bottom() {
                assert!(row(buffer, output_y).contains("Fixture"));
                assert_eq!(buffer[(area.x + 1, output_y)].bg, palette.base);
            }
            assert_eq!(buffer[(area.x, geometry.editor.y)].bg, palette.band);
            assert_eq!(app.editor.text(), "editable draft");
            assert!(
                geometry
                    .editor
                    .contains(app.editor.cursor().unwrap().into())
            );
            assert_eq!(buffer[(0, 0)].symbol(), " ");
        }
    }
}

#[test]
fn ts3_wrapped_user_band_clips_and_scrolls_without_false_edges_or_repeated_prompt() {
    use ratatui::layout::Rect;
    for light in [false, true] {
        let mut app = App::new(true);
        app.fixture.transcript.clear();
        app.fixture
            .submit(format!("{}END", "wrapped 界 e\u{301} 👩🏽‍💻 ".repeat(250)), 0)
            .unwrap();
        app.fixture.acknowledge_pending();
        app.viewport.scroll_rows(-10000);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let completed = terminal
            .draw(|frame| ui::draw(frame, &mut app, light))
            .unwrap();
        let geometry = ui::app_geometry(&app, Rect::new(0, 0, 80, 24));
        let bottom = geometry.transcript.bottom() - 1;
        let p = Palette::new(light);
        assert_eq!(completed.buffer[(0, bottom)].symbol(), " ");
        assert_eq!(completed.buffer[(79, bottom)].bg, p.user_band);
        app.viewport.scroll_rows(5);
        let completed = terminal
            .draw(|frame| ui::draw(frame, &mut app, light))
            .unwrap();
        let buffer = completed.buffer;
        for y in geometry.transcript.y..geometry.transcript.bottom() {
            assert_eq!(buffer[(0, y)].bg, p.user_band);
            assert_eq!(buffer[(79, y)].bg, p.user_band);
            assert_eq!(buffer[(1, y)].symbol(), " ");
            assert!(!row(buffer, y).contains('▀'));
        }
        app.viewport.follow_latest();
        terminal
            .draw(|frame| ui::draw(frame, &mut app, light))
            .unwrap();
        let geometry = ui::app_geometry(&app, Rect::new(0, 0, 80, 24));
        let visible = (geometry.transcript.y..geometry.transcript.bottom())
            .map(|y| row(terminal.backend().buffer(), y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(visible.contains("END"));
        assert!(visible.contains("Message received"));
    }
}

#[test]
fn ts3_lower_strip_and_separator_are_real_scroll_positions_across_resize() {
    for light in [false, true] {
        let mut app = App::new(true);
        app.fixture.transcript.clear();
        app.fixture.submit("alpha\nbeta".into(), 0).unwrap();
        app.fixture.acknowledge_pending();
        // Delivered output after the input lets every part of its band reach
        // the top of the viewport without manufacturing transcript entries.
        for _ in 0..4 {
            app.fixture.set_connected(false);
            app.fixture.set_connected(true);
        }
        let p = Palette::new(light);
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        app.viewport.scroll_rows(-1000);
        terminal
            .draw(|frame| ui::draw(frame, &mut app, light))
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(0, 1)].symbol(), "▄");
        app.viewport.scroll_rows(3);
        terminal
            .draw(|frame| ui::draw(frame, &mut app, light))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 1)].symbol(), "▀");
        assert_eq!(buffer[(0, 1)].fg, p.user_band);
        assert_eq!(buffer[(0, 1)].bg, p.base);
        app.viewport.scroll_rows(1);
        terminal
            .draw(|frame| ui::draw(frame, &mut app, light))
            .unwrap();
        assert!(row(terminal.backend().buffer(), 1).contains("Fixture"));
        app.viewport.scroll_rows(1);
        terminal
            .draw(|frame| ui::draw(frame, &mut app, light))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(row(buffer, 1).trim(), "");
        assert_eq!(buffer[(0, 1)].bg, p.base);
        assert!(row(buffer, 2).contains("Fixture"));
        for (width, height) in [(59, 16), (60, 15), (60, 16)] {
            let mut resized = Terminal::new(TestBackend::new(width, height)).unwrap();
            resized
                .draw(|frame| ui::draw(frame, &mut app, light))
                .unwrap();
            let buffer = resized.backend().buffer();
            // The retained output separator stays at the top across the
            // user-band decoration thresholds.
            assert!(row(buffer, 1).trim().is_empty());
            assert!(app.notice.is_empty());
        }
    }
}

#[test]
fn tp1_composer_insets_tint_cursor_and_status_survive_notice_and_resize() {
    let text = "first\ne\u{301} 👩🏽‍💻\n界面";
    for light in [false, true] {
        let mut app = App::default();
        app.editor.insert(text).unwrap();
        for (width, height) in [(120, 40), (80, 24), (40, 12), (80, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| ui::draw(frame, &mut app, light))
                .unwrap();
            let strips = u16::from(width >= 60 && height >= 16);
            let editor_y = height - 1 - strips - 3;
            let palette = Palette::new(light);
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer[(1, editor_y)].symbol(), "›");
            assert_eq!(buffer[(3, editor_y)].symbol(), "f");
            assert_eq!(buffer[(3, editor_y + 1)].symbol(), "e\u{301}");
            assert_eq!(buffer[(3, editor_y + 2)].symbol(), "界");
            for y in editor_y..editor_y + 3 {
                assert_eq!(buffer[(0, y)].bg, palette.band);
                assert_eq!(buffer[(width - 1, y)].bg, palette.band);
            }
            assert_eq!(buffer[(0, height - 1)].bg, palette.base);
            assert_eq!(app.editor.cursor(), Some((7, editor_y + 2)));
            app.notice = "A changing notice must leave the draft and cursor where they are.".into();
            terminal
                .draw(|frame| ui::draw(frame, &mut app, light))
                .unwrap();
            assert_eq!(app.editor.text(), text);
            assert_eq!(app.editor.cursor(), Some((7, editor_y + 2)));
        }
    }
}

#[test]
fn tp1_reading_anchor_survives_real_fixture_retention() {
    let mut app = App::default();
    for index in 0..110 {
        app.fixture.submit(format!("message {index}"), 0).unwrap();
        app.fixture.acknowledge_pending();
        app.fixture.complete(0);
    }
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    app.viewport.scroll_rows(-24);
    terminal
        .draw(|frame| ui::draw(frame, &mut app, false))
        .unwrap();
    let before: Vec<_> = (1..8)
        .map(|y| row(terminal.backend().buffer(), y))
        .collect();
    app.handle(Event::Key(KeyEvent::new(
        KeyCode::Char('p'),
        KeyModifiers::CONTROL,
    )));
    app.handle(Event::Paste("Independent Observatory draft".into()));
    terminal
        .draw(|frame| ui::draw(frame, &mut app, false))
        .unwrap();
    app.handle(Event::Key(KeyEvent::new(
        KeyCode::Char('p'),
        KeyModifiers::CONTROL,
    )));
    app.fixture.submit("next message".into(), 1).unwrap();
    app.fixture.acknowledge_pending();
    terminal
        .draw(|frame| ui::draw(frame, &mut app, false))
        .unwrap();
    let after: Vec<_> = (1..8)
        .map(|y| row(terminal.backend().buffer(), y))
        .collect();
    assert_eq!(before, after);
}

#[test]
fn tp1_latest_output_is_reachable_after_a_maximum_multiline_message() {
    let mut app = App::default();
    app.fixture
        .submit(format!("a{}", "\n".repeat(65_535)), 0)
        .unwrap();
    app.fixture.acknowledge_pending();
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal
        .draw(|frame| ui::draw(frame, &mut app, false))
        .unwrap();
    let visible = (0..12)
        .map(|y| row(terminal.backend().buffer(), y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(visible.contains("Message received"), "{visible}");
}

#[test]
#[ignore = "manual responsiveness measurement, not a timing-sensitive unit gate"]
fn tp1_responsiveness_measurement() {
    let mut app = App::default();
    for index in 0..100 {
        app.fixture
            .submit(
                format!("Fixture transcript entry {index}: a representative completed message."),
                0,
            )
            .unwrap();
        app.fixture.acknowledge_pending();
        app.fixture.complete(0);
    }
    app.fixture.submit("Active Studio work".into(), 0).unwrap();
    app.fixture.acknowledge_pending();
    app.handle(Event::Key(KeyEvent::new(
        KeyCode::Char('p'),
        KeyModifiers::CONTROL,
    )));
    app.fixture
        .submit("Active Observatory work".into(), 0)
        .unwrap();
    app.fixture.acknowledge_pending();
    app.handle(Event::Key(KeyEvent::new(
        KeyCode::Char('p'),
        KeyModifiers::CONTROL,
    )));
    for (action, text) in [
        (Action::Steer, "Keep the interaction responsive"),
        (Action::Queue, "Explore command discovery after this work"),
        (Action::Queue, "Review the interaction and spacing"),
    ] {
        app.fixture
            .submit_request(app.fixture.target(), action, 0, text.into(), 0)
            .unwrap();
        app.fixture.acknowledge_pending();
    }
    assert_eq!(app.fixture.display_queued_count(), Some(2));
    assert!(app.fixture.message_tray().items.len() >= 4);
    app.editor
        .insert("A representative multiline draft\nwith e\u{301}, 👩🏽‍💻 and 界面.")
        .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| ui::draw(frame, &mut app, false))
        .unwrap();
    let mut samples = Vec::with_capacity(1000);
    for iteration in 0..1000 {
        let start = std::time::Instant::now();
        app.advance(iteration * 5);
        if iteration % 125 == 0 {
            app.handle(Event::Key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE)));
        }
        for _ in 0..15 {
            app.handle(Event::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )));
            app.handle(Event::Key(KeyEvent::new(
                KeyCode::Backspace,
                KeyModifiers::NONE,
            )));
        }
        terminal
            .draw(|frame| ui::draw(frame, &mut app, false))
            .unwrap();
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "120x40; 1000 batches of 30 editing events + both fixture ticks + paint; periodic decision changes; 200 transcript entries; retained message tray with Steer and two queued messages; debug profile"
    );
    println!(
        "p95={:.3} ms; max={:.3} ms; target p95 <50 ms",
        samples[949], samples[999]
    );
}
