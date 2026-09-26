//! GT1/GT2 through the actual editor, fixture, input route and renderer.

use asura_tui_chat::{
    app::{App, Overlay},
    model::{MessageState, Project},
    tour::{SCENE_COUNT, Tour},
    ui::{self, Palette},
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

fn event(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn render(tour: &mut Tour, width: u16, height: u16, light: bool) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| tour.draw(frame, light)).unwrap();
    terminal.backend().buffer().clone()
}

fn visible(buffer: &Buffer) -> String {
    let area = buffer.area;
    (area.y..area.bottom())
        .map(|y| row(buffer, y))
        .collect::<Vec<_>>()
        .join("\n")
}

fn row(buffer: &Buffer, y: u16) -> String {
    let area = buffer.area;
    (area.x..area.right())
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

fn move_to(tour: &mut Tour, index: usize) {
    while tour.index() < index {
        assert!(!tour.handle(event(KeyCode::Right)).unwrap());
    }
}

#[test]
fn gt1_all_checked_scenes_render_at_both_palettes_and_three_trial_sizes() {
    for light in [false, true] {
        for (width, height) in [(120, 40), (80, 24), (40, 12)] {
            let mut tour = Tour::new().unwrap();
            for index in 0..SCENE_COUNT {
                move_to(&mut tour, index);
                let draft = tour.app().editor.text();
                let overlay = tour.app().overlay.clone();
                let buffer = render(&mut tour, width, height, light);
                let screen = visible(&buffer);
                assert!(row(&buffer, 1).contains("ASURA prototype"), "{screen}");
                let mut expected_footer =
                    format!("Tour {}/{} N/P L H help Q quit", index + 1, SCENE_COUNT);
                if width >= 60 {
                    expected_footer.push_str(" · editing locked · ");
                    expected_footer.push_str(tour.title());
                }
                assert_eq!(row(&buffer, 0).trim(), expected_footer, "{screen}");
                assert!(!row(&buffer, height - 1).contains("Tour "), "{screen}");
                assert!(!row(&buffer, height - 1).trim().is_empty(), "{screen}");
                assert_eq!(buffer[(0, height - 1)].bg, Palette::new(light).base);
                assert_eq!(tour.app().editor.text(), draft);
                assert_eq!(tour.app().overlay, overlay);
                if overlay.is_none() {
                    let (x, y) = tour.app().editor.cursor().expect("editor cursor in tour");
                    assert!(x >= 3 && x < width && y >= 2 && y < height - 1);
                }
                match index {
                    0 => {
                        assert!(screen.contains("No accidental send."), "{screen}");
                        assert!(tour.app().fixture.pending.is_none());
                    }
                    1 => {
                        assert!(
                            screen.contains("Steer") && screen.contains("Queue"),
                            "{screen}"
                        );
                        assert!(matches!(
                            tour.app().overlay,
                            Some(Overlay::Submission { selected: None, .. })
                        ));
                    }
                    2 => assert!(screen.contains("unavailable"), "{screen}"),
                    3 => {
                        assert_eq!(tour.app().project(), Project::Observatory);
                        let activity = if width == 40 {
                            "Studio: workin"
                        } else {
                            "Studio: working"
                        };
                        assert!(row(&buffer, 1).contains(activity), "{screen}");
                        assert!(screen.contains("Observatory draft"), "{screen}");
                    }
                    4 => {
                        assert!(screen.contains("Newer draft"), "{screen}");
                        assert!(screen.contains("Rejected original"), "{screen}");
                        assert!(tour.app().fixture.recoverable.is_empty());
                    }
                    5 => {
                        assert!(screen.contains("Disconnected"), "{screen}");
                        assert!(screen.lines().last().unwrap().contains("?%"), "{screen}");
                        assert!(screen.contains("Unknown:"), "{screen}");
                        assert!(
                            !tour
                                .app()
                                .fixture
                                .transcript
                                .iter()
                                .any(|line| line.text.contains("Accepted before disconnect")),
                            "{screen}"
                        );
                        assert!(
                            tour.app()
                                .fixture
                                .message_tray()
                                .items
                                .iter()
                                .any(|item| item.state == MessageState::Unknown)
                        );
                        assert!(tour.app().fixture.display_task().is_none());
                    }
                    6 => {
                        assert!(tour.app().fixture.connected);
                        assert!(tour.app().fixture.pending.is_none());
                        assert!(screen.contains("Independent draft"), "{screen}");
                    }
                    7 => assert!(matches!(
                        tour.app().overlay,
                        Some(Overlay::Reset { selected: false })
                    )),
                    8 => {
                        assert_eq!(tour.app().fixture.display_queued_count(), Some(2));
                        let status = tour.app().composer_status();
                        let footer = screen.lines().last().unwrap();
                        assert!(footer.contains(status.spinner.unwrap()), "{screen}");
                        assert!(footer.contains("18%"), "{screen}");
                        assert!(
                            footer.contains(&format!(
                                "{}{}",
                                status.characters,
                                if width < 60 { "c" } else { " chars" }
                            )),
                            "{screen}"
                        );
                        if width >= 60 {
                            assert!(screen.contains("Steer · Acknowledged:"), "{screen}");
                            assert!(screen.contains("Queued: Explore command"), "{screen}");
                            assert!(screen.contains("Queued: Then review"), "{screen}");
                        }
                    }
                    9 => {
                        assert!(matches!(
                            tour.app().overlay,
                            Some(Overlay::MessageInspector { .. })
                        ));
                        assert!(screen.contains("Completed"), "{screen}");
                    }
                    _ => unreachable!(),
                }
            }
        }
    }
}

#[test]
fn gt2_compact_overlay_hints_and_background_header_preserve_the_ordinary_path() {
    for light in [false, true] {
        let mut ordinary = App::new(true);
        ordinary.handle(Event::Paste("Start".into()));
        ordinary.handle(event(KeyCode::Enter));
        ordinary.handle(event(KeyCode::F(5)));
        ordinary.handle(Event::Paste("Refine this, or save it for later".into()));
        ordinary.handle(event(KeyCode::Enter));
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|frame| ui::draw(frame, &mut ordinary, light))
            .unwrap();
        let normal = terminal.backend().buffer().clone();
        let normal_screen = visible(&normal);
        assert!(
            normal_screen.contains("Esc closes · PgUp/PgDn"),
            "{normal_screen}"
        );
        assert!(!normal_screen.contains("Locked · H help Q quit"));

        let mut tour = Tour::new().unwrap();
        move_to(&mut tour, 1);
        let buffer = render(&mut tour, 40, 12, light);
        let screen = visible(&buffer);
        assert!(screen.contains("Locked · H help Q quit"), "{screen}");
        assert!(!screen.contains("Esc closes · PgUp/PgDn"));
        assert_eq!(row(&buffer, 1), row(&normal, 0));
        move_to(&mut tour, 3);
        let buffer = render(&mut tour, 40, 12, light);
        assert!(row(&buffer, 1).contains("Studio: workin"));
    }
}

#[test]
fn gt2_help_theme_and_ignored_input_preserve_captured_application_focus() {
    let mut tour = Tour::new().unwrap();
    move_to(&mut tour, 2);
    render(&mut tour, 40, 12, false);
    let overlay = tour.app().overlay.clone();
    let draft = tour.app().editor.text();
    let transcript = tour.app().fixture.transcript.clone();
    tour.handle(event(KeyCode::Char('h'))).unwrap();
    let screen = visible(&render(&mut tour, 40, 12, false));
    assert!(screen.contains("Tour help"), "{screen}");
    assert!(screen.contains("Expired decision"), "{screen}");
    for _ in 0..30 {
        tour.handle(event(KeyCode::PageDown)).unwrap();
    }
    let screen = visible(&render(&mut tour, 40, 12, false));
    assert!(screen.contains("observations separately"), "{screen}");
    assert!(screen.contains("H/Esc close · Q quit"), "{screen}");
    tour.handle(Event::Paste("q\nn\nexternal text".into()))
        .unwrap();
    tour.handle(event(KeyCode::Enter)).unwrap();
    tour.handle(event(KeyCode::Char('n'))).unwrap();
    assert_eq!(tour.index(), 2);
    assert!(!tour.handle(event(KeyCode::Esc)).unwrap());
    tour.handle(event(KeyCode::Char('l'))).unwrap();
    let buffer = render(&mut tour, 40, 12, false);
    assert_eq!(buffer[(0, 11)].bg, Palette::new(true).base);
    let buffer = render(&mut tour, 40, 12, true);
    assert_eq!(buffer[(0, 11)].bg, Palette::new(false).base);
    assert_eq!(tour.app().overlay, overlay);
    assert_eq!(tour.app().editor.text(), draft);
    assert_eq!(tour.app().fixture.transcript, transcript);
}

#[test]
fn gt2_tiny_rendering_retains_the_scene_and_minimum_geometry_keeps_controls() {
    let mut tour = Tour::new().unwrap();
    move_to(&mut tour, 7);
    let original = tour.app().editor.text();
    let overlay = tour.app().overlay.clone();
    for (width, height) in [(29, 7), (0, 0), (30, 7), (29, 8), (30, 8)] {
        let buffer = render(&mut tour, width, height, false);
        if width >= 29 && height >= 7 {
            let screen = visible(&buffer);
            assert!(screen.contains("Resize to 30 × 9"), "{screen}");
            assert!(screen.contains("Q quits"), "{screen}");
        }
        for code in [KeyCode::Char('r'), KeyCode::Left, KeyCode::Char('n')] {
            assert!(!tour.handle(event(code)).unwrap());
        }
        assert_eq!(tour.index(), 7);
        assert_eq!(tour.app().editor.text(), original);
        assert_eq!(tour.app().overlay, overlay);
    }
    let buffer = render(&mut tour, 30, 9, false);
    let screen = visible(&buffer);
    assert!(
        row(&buffer, 0).contains("Tour 8/10 N/P L H Q quit"),
        "{screen}"
    );
    assert!(screen.contains("Locked · H help Q quit"), "{screen}");
    move_to(&mut tour, SCENE_COUNT - 1);
    let buffer = render(&mut tour, 30, 9, false);
    assert_eq!(row(&buffer, 0).trim(), "Tour 10/10 N/P L H Q quit");
    assert!(!row(&buffer, 8).trim().is_empty());
    tour.handle(event(KeyCode::Char('h'))).unwrap();
    let screen = visible(&render(&mut tour, 30, 9, false));
    assert!(screen.contains("H/Esc close · Q quit"), "{screen}");
    render(&mut tour, 20, 4, false);
    assert!(!tour.handle(event(KeyCode::Esc)).unwrap());
    render(&mut tour, 80, 24, false);
    tour.handle(event(KeyCode::Char('p'))).unwrap();
    assert_eq!(tour.index(), SCENE_COUNT - 2);
    assert!(tour.handle(event(KeyCode::Char('q'))).unwrap());
}
