//! GT4: optional artifacts from actual rendered cells, never native-terminal proof.

use std::{fmt::Write as _, fs, path::Path};

use asura_tui_chat::{
    tour::{SCENE_COUNT, Tour},
    ui::Palette,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Terminal,
    backend::TestBackend,
    buffer::Buffer,
    style::{Color, Modifier},
};

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn rgb(color: Color, fallback: &str) -> String {
    let (r, g, b) = match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Reset => return fallback.into(),
        Color::Indexed(index) if index >= 232 => {
            let gray = 8 + (index - 232) * 10;
            (gray, gray, gray)
        }
        Color::Indexed(index) if index >= 16 => {
            let offset = index - 16;
            let channel = |value: u8| if value == 0 { 0 } else { 55 + 40 * value };
            (
                channel(offset / 36),
                channel((offset / 6) % 6),
                channel(offset % 6),
            )
        }
        other => {
            let index = match other {
                Color::Black => 0,
                Color::Red => 1,
                Color::Green => 2,
                Color::Yellow => 3,
                Color::Blue => 4,
                Color::Magenta => 5,
                Color::Cyan => 6,
                Color::Gray => 7,
                Color::DarkGray => 8,
                Color::LightRed => 9,
                Color::LightGreen => 10,
                Color::LightYellow => 11,
                Color::LightBlue => 12,
                Color::LightMagenta => 13,
                Color::LightCyan => 14,
                Color::White => 15,
                Color::Indexed(index) => usize::from(index),
                _ => unreachable!("RGB, reset and higher indexed colors handled above"),
            };
            const ANSI: [&str; 16] = [
                "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080",
                "#c0c0c0", "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff",
                "#00ffff", "#ffffff",
            ];
            return ANSI[index].into();
        }
    };
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn cells(buffer: &Buffer, light: bool) -> String {
    let mut backgrounds = String::new();
    let mut foregrounds = String::new();
    let palette = Palette::new(light);
    let default_fg = rgb(palette.text, "inherit");
    let default_bg = rgb(palette.base, if light { "#ffffff" } else { "#111111" });
    for position in buffer.area.positions() {
        let (x, y) = (position.x, position.y);
        let cell = &buffer[position];
        let mut fg = rgb(cell.fg, &default_fg);
        let mut bg = rgb(cell.bg, &default_bg);
        if cell.modifier.contains(Modifier::REVERSED) {
            std::mem::swap(&mut fg, &mut bg);
        }
        let position = format!("left:{}px;top:{}px;", x * 9, y * 20);
        write!(backgrounds, "<i style='{position}background:{bg}'></i>").unwrap();
        if cell.symbol() == " " || cell.modifier.contains(Modifier::HIDDEN) {
            continue;
        }
        let weight = if cell.modifier.contains(Modifier::BOLD) {
            "bold"
        } else {
            "normal"
        };
        let slant = if cell.modifier.contains(Modifier::ITALIC) {
            "italic"
        } else {
            "normal"
        };
        let mut decorations = Vec::new();
        if cell.modifier.contains(Modifier::UNDERLINED) {
            decorations.push("underline");
        }
        if cell.modifier.contains(Modifier::CROSSED_OUT) {
            decorations.push("line-through");
        }
        let opacity = if cell.modifier.contains(Modifier::DIM) {
            "0.65"
        } else {
            "1"
        };
        write!(
            foregrounds,
            "<span style='{position}color:{fg};font-weight:{weight};font-style:{slant};text-decoration:{};opacity:{opacity}'>{}</span>",
            decorations.join(" "),
            escape(cell.symbol())
        )
        .unwrap();
    }
    // Draw all cell backgrounds before wide/combined glyphs, so continuation
    // cells cannot erase their left-hand glyph. Browser shaping remains synthetic.
    backgrounds + &foregrounds
}

#[test]
fn preview_text_cannot_become_markup_and_colors_cover_terminal_ranges() {
    assert_eq!(escape("<&\"'>"), "&lt;&amp;&quot;&#39;&gt;");
    assert_eq!(rgb(Color::Rgb(20, 25, 23), ""), "#141917");
    assert_eq!(rgb(Color::Indexed(16), ""), "#000000");
    assert_eq!(rgb(Color::Indexed(231), ""), "#ffffff");
    assert_eq!(rgb(Color::Indexed(255), ""), "#eeeeee");
    assert_eq!(rgb(Color::Reset, "#abcdef"), "#abcdef");
    let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, 2, 1));
    buffer[(0, 0)].set_symbol("<").set_style(
        ratatui::style::Style::default()
            .fg(Color::Rgb(1, 2, 3))
            .bg(Color::Rgb(4, 5, 6))
            .add_modifier(Modifier::REVERSED | Modifier::BOLD),
    );
    let html = cells(&buffer, false);
    assert!(html.contains("background:#010203"));
    assert!(html.contains("color:#040506;font-weight:bold"));
    assert!(html.contains("&lt;</span>"));
}

#[test]
#[ignore = "writes synthetic HTML previews to ignored target/tour-previews"]
fn export_tour_previews() -> Result<(), Box<dyn std::error::Error>> {
    let destination = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tour-previews");
    fs::create_dir_all(&destination)?;
    for (width, height) in [(120, 40), (80, 24), (40, 12)] {
        for light in [false, true] {
            let palette = if light { "light" } else { "dark" };
            let mut html = String::from(
                "<!doctype html><meta charset='utf-8'><title>Asura synthetic renderer review</title>\
                 <style>body{margin:16px;background:#c8cbc9;font:14px system-ui}\
                 section{padding:12px;margin-bottom:18px;width:max-content;background:#eee}\
                 h2{font-size:16px;margin:0 0 4px}p{margin:0 0 10px}\
                 .screen{position:relative;font:15px/20px Menlo,monospace;white-space:pre}\
                 .screen i,.screen span{position:absolute;display:block}\
                 .screen i{width:9px;height:20px}</style>",
            );
            let mut tour = Tour::new().map_err(std::io::Error::other)?;
            for index in 0..SCENE_COUNT {
                assert_eq!(tour.index(), index);
                let mut terminal = Terminal::new(TestBackend::new(width, height))?;
                let completed = terminal.draw(|frame| tour.draw(frame, light))?;
                write!(
                    html,
                    "<section id='scene-{}'><h2>{}/{} · {}</h2><p>Synthetic Ratatui cells · {}×{} · {} · assumed terminal background {} · not native terminal evidence</p><div class='screen' style='width:{}px;height:{}px'>{}</div></section>",
                    index + 1,
                    index + 1,
                    SCENE_COUNT,
                    escape(tour.title()),
                    width,
                    height,
                    palette,
                    if light { "#ffffff" } else { "#111111" },
                    width * 9,
                    height * 20,
                    cells(completed.buffer, light),
                )?;
                if index + 1 < SCENE_COUNT {
                    let exit = tour
                        .handle(Event::Key(KeyEvent::new(
                            KeyCode::Char('n'),
                            KeyModifiers::NONE,
                        )))
                        .map_err(std::io::Error::other)?;
                    assert!(!exit);
                }
            }
            fs::write(
                destination.join(format!("tour-{width}x{height}-{palette}.html")),
                html,
            )?;
        }
    }
    Ok(())
}
