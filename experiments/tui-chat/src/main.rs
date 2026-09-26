use asura_tui_chat::{
    app::App,
    key_probe::KeyProbe,
    terminal::{self, Session},
    tour::Tour,
    ui,
};
use crossterm::event;
use ratatui::{Frame, Terminal, backend::CrosstermBackend};
use std::{
    io,
    time::{Duration, Instant},
};

enum Surface {
    Chat(App),
    Tour(Tour),
    Keys(KeyProbe),
}

impl Surface {
    fn draw(&mut self, frame: &mut Frame<'_>, light: bool) {
        match self {
            Self::Chat(app) => ui::draw(frame, app, light),
            Self::Tour(tour) => tour.draw(frame, light),
            Self::Keys(probe) => probe.draw(frame, light),
        }
    }

    fn advance(&mut self, elapsed: u64) -> bool {
        match self {
            Self::Chat(app) => app.advance(elapsed),
            Self::Tour(_) | Self::Keys(_) => false,
        }
    }

    fn handle(&mut self, event: event::Event) -> io::Result<bool> {
        match self {
            Self::Chat(app) => {
                app.handle(event);
                Ok(app.exit)
            }
            Self::Tour(tour) => tour.handle(event).map_err(io::Error::other),
            Self::Keys(probe) => Ok(probe.handle(event)),
        }
    }
}

fn run() -> io::Result<()> {
    let mut light = false;
    let mut fault = None;
    let mut manual = false;
    let mut tour_requested = false;
    let mut keys_requested = false;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--light" => light = true,
            "--manual" => manual = true,
            "--tour" => tour_requested = true,
            "--keys" => keys_requested = true,
            "--fault=setup" => fault = Some("setup"),
            "--fault=draw" => fault = Some("draw"),
            "--fault=panic" => fault = Some("panic"),
            "--fault=tour" => fault = Some("tour"),
            "--help" | "-h" => {
                println!(
                    "Asura chat prototype (synthetic, offline)\nUsage: asura-tui-chat [--light] [--manual] [--tour | --keys]\nChat: Enter sends; Option+Return newline; F1 help; Ctrl+Q exit.\nTour: N/P next/previous; L palette; H help; R restart; Q exit.\nKeys: inert key-event inspection; Escape or Ctrl+Q exits."
                );
                return Ok(());
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Unknown argument: {argument}"),
                ));
            }
        }
    }
    if keys_requested && tour_requested {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Cannot combine --keys and --tour",
        ));
    }
    if fault == Some("tour") && !tour_requested {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--fault=tour requires --tour",
        ));
    }
    terminal::install_panic_hook();
    let mut session = if fault == Some("setup") {
        Session::enter_with_setup_fault()?
    } else {
        Session::enter()?
    };
    let result = (|| {
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        let mut surface = if tour_requested {
            Surface::Tour(Tour::new().map_err(io::Error::other)?)
        } else if keys_requested {
            Surface::Keys(KeyProbe::new())
        } else {
            Surface::Chat(App::new(manual))
        };
        let started = Instant::now();
        terminal.clear()?;
        terminal.draw(|frame| surface.draw(frame, light))?;
        if fault == Some("draw") {
            return Err(io::Error::other("injected render I/O failure"));
        }
        if fault == Some("panic") {
            panic!("injected terminal cleanup qualification panic");
        }
        if fault == Some("tour") {
            return Err(io::Error::other("injected tour checkpoint failure"));
        }
        let mut exit = false;
        while !exit && !session.interrupted() {
            let mut changed =
                surface.advance(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
            if event::poll(Duration::from_millis(16))? {
                changed = true;
                for _ in 0..32 {
                    exit = surface.handle(event::read()?)?;
                    if exit || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
            if changed {
                terminal.draw(|frame| surface.draw(frame, light))?;
            }
        }
        Ok(())
    })();
    let cleanup = session.restore();
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(io::Error::new(
            error.kind(),
            format!("{error}; terminal cleanup also failed: {cleanup}"),
        )),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("asura-tui-chat: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
