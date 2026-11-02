//! `asura llamacpp …` (llamacpp-backend.md, configuration and commands): the
//! person's commands for Asura's own llama.cpp server, and the hidden
//! `serve` step that launchd runs. Each is a one-shot process; its waits (a
//! `launchctl` child, file IO, the Keychain) end on completion or the
//! person's interrupt. None needs the service.
use asura_platform::RuntimeDirectory;
use asura_platform::llamacpp::{self as server, AgentState, Launchctl, Started};
use asura_storage::config::{LlamacppServer, LlamacppSettings};
use asura_storage::llamacpp::{self as files, ApprovalState, Paths};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Approve(String),
    Add(String),
    Remove(String),
    List,
    Start,
    Stop,
    Status,
    /// Hidden: run by launchd only.
    Serve,
}

pub(crate) fn parse(words: &[&str]) -> Option<Command> {
    Some(match words {
        ["approve", path] => Command::Approve((*path).into()),
        ["add", path] => Command::Add((*path).into()),
        ["remove", name] => Command::Remove((*name).into()),
        ["list"] => Command::List,
        ["start"] => Command::Start,
        ["stop"] => Command::Stop,
        ["status"] => Command::Status,
        ["serve"] => Command::Serve,
        _ => return None,
    })
}

pub(crate) const USAGE: &str = "  asura llamacpp approve PATH\n  asura llamacpp add PATH\n  asura llamacpp remove NAME\n  asura llamacpp list\n  asura llamacpp start\n  asura llamacpp stop\n  asura llamacpp status";

/// The account home that holds `~/.asura` (path projection only).
fn home(paths: &Paths) -> PathBuf {
    paths
        .logs
        .parent()
        .and_then(Path::parent)
        .expect("logs sit in ~/.asura")
        .to_path_buf()
}

fn settings(runtime: &RuntimeDirectory) -> Result<LlamacppSettings, &'static str> {
    asura_storage::config::llamacpp_settings(runtime.clone(), &AtomicBool::new(false))
}

fn short(hex: &str) -> &str {
    &hex[..12.min(hex.len())]
}

/// Restarts a loaded agent, saying so first: the router reads its folder
/// only at start.
fn restart_if_loaded(launchctl: &dyn Launchctl, uid: u32, out: &mut dyn Write) {
    if let Ok(AgentState::Loaded { .. }) = server::agent_state(launchctl, uid) {
        let _ = writeln!(
            out,
            "Restarting Asura's llama.cpp server to read the model folder."
        );
        if let Err(error) = server::restart(launchctl, uid) {
            let _ = writeln!(out, "The restart failed: {error}");
        }
    }
}

/// Runs one person command. Returns the exit status: 0 done, 1 refused.
pub(crate) fn run(
    command: Command,
    runtime: &RuntimeDirectory,
    uid: u32,
    program: &Path,
    launchctl: &dyn Launchctl,
    out: &mut dyn Write,
) -> i32 {
    let paths = Paths::of(runtime);
    let refuse = |out: &mut dyn Write, text: String| {
        let _ = writeln!(out, "{text}");
        1
    };
    match command {
        Command::Approve(path) => match files::approve(&paths, Path::new(&path)) {
            Ok((approval, previous)) => {
                if let Some(previous) = previous.filter(|p| p.sha256 != approval.sha256) {
                    let _ = writeln!(
                        out,
                        "Replaced the approval of {} ({}).",
                        previous.path.display(),
                        short(&previous.digest_hex())
                    );
                }
                let _ = writeln!(
                    out,
                    "Approved {} ({}), SHA-256 {}.",
                    approval.path.display(),
                    approval.real_path.display(),
                    short(&approval.digest_hex())
                );
                restart_if_loaded(launchctl, uid, out);
                0
            }
            Err(error) => refuse(out, format!("Not approved: {}.", error.code())),
        },
        Command::Add(path) => match files::add(&paths, Path::new(&path)) {
            Ok(name) => {
                let _ = writeln!(out, "Added llamacpp:{name}. The file stays at {path}.");
                restart_if_loaded(launchctl, uid, out);
                0
            }
            Err(error) => refuse(out, format!("Not added: {}.", error.code())),
        },
        Command::Remove(name) => match files::remove(&paths, &name) {
            Ok(target) => {
                let _ = writeln!(
                    out,
                    "Removed llamacpp:{name}.{}",
                    target
                        .map(|t| format!(" The file stays at {}.", t.display()))
                        .unwrap_or_default()
                );
                restart_if_loaded(launchctl, uid, out);
                0
            }
            Err(_) => refuse(out, format!("No model link named {name}.")),
        },
        Command::List => match files::list(&paths) {
            Ok(links) => {
                if links.is_empty() {
                    let _ = writeln!(out, "No models. Add one with asura llamacpp add PATH.");
                }
                for link in links {
                    let _ = match link.size {
                        Some(size) => writeln!(out, "llamacpp:{}  {size} bytes", link.name),
                        None => writeln!(out, "llamacpp:{}  broken link", link.name),
                    };
                }
                0
            }
            Err(error) => refuse(out, format!("The model folder cannot be read: {error}.")),
        },
        Command::Start => {
            let settings = match settings(runtime) {
                Ok(settings) => settings,
                Err(code) => {
                    return refuse(out, format!("The configuration cannot be read: {code}."));
                }
            };
            if settings.server != LlamacppServer::Asura {
                return refuse(
                    out,
                    "providers.llamacpp.server is not asura; set it with /config set providers.llamacpp.server asura.".into(),
                );
            }
            if let Some(reason) = start_refusal(&paths) {
                return refuse(out, reason);
            }
            let _ = writeln!(
                out,
                "Starting Asura's llama.cpp server as the LaunchAgent {} on 127.0.0.1:{}.",
                server::LABEL,
                settings.port
            );
            match server::start(launchctl, uid, &home(&paths), program) {
                Ok(Started::Registered) => 0,
                Ok(Started::Restarted) => {
                    let _ = writeln!(out, "It was running; it restarted.");
                    0
                }
                Err(error) => refuse(out, format!("launchctl failed: {error}.")),
            }
        }
        Command::Stop => match server::stop(launchctl, uid, &home(&paths)) {
            Ok(true) => {
                let _ = writeln!(
                    out,
                    "Stopped Asura's llama.cpp server and removed its LaunchAgent."
                );
                0
            }
            Ok(false) => {
                let _ = writeln!(out, "Asura's llama.cpp server was not registered.");
                0
            }
            Err(error) => refuse(out, format!("launchctl failed: {error}.")),
        },
        Command::Status => {
            let settings = match settings(runtime) {
                Ok(settings) => settings,
                Err(code) => {
                    return refuse(out, format!("The configuration cannot be read: {code}."));
                }
            };
            let _ = writeln!(out, "Mode: {}", settings.server.name());
            let _ = writeln!(
                out,
                "Port: {}; context {} tokens; {} slot(s); {} model(s) loaded at most; sleep after {} s",
                settings.port,
                settings.context_length,
                settings.slots,
                settings.models_loaded,
                settings.idle_seconds
            );
            let _ = writeln!(
                out,
                "Binary: {}",
                match files::check(&paths) {
                    Ok(ApprovalState::Unset) => "not approved".to_owned(),
                    Ok(ApprovalState::Approved(a)) =>
                        format!("approved {} ({})", a.path.display(), short(&a.digest_hex())),
                    Ok(ApprovalState::Changed { record, current }) => format!(
                        "changed: approved {} is now {}",
                        short(&record.digest_hex()),
                        short(&current.digest_hex())
                    ),
                    Ok(ApprovalState::Missing(a)) => format!("missing: {}", a.path.display()),
                    Err(error) => format!("unreadable: {error}"),
                }
            );
            let models = files::list(&paths).map(|l| l.len()).unwrap_or(0);
            let _ = writeln!(out, "Models: {models}");
            let _ = writeln!(
                out,
                "LaunchAgent: {}",
                match server::agent_state(launchctl, uid) {
                    Ok(AgentState::Loaded { pid: Some(pid), .. }) => format!("running, pid {pid}"),
                    Ok(AgentState::Loaded { last_exit, .. }) => format!(
                        "loaded, not running (last exit {})",
                        last_exit.map_or("unknown".into(), |c| c.to_string())
                    ),
                    Ok(AgentState::NotLoaded) => "not registered".into(),
                    Err(error) => format!("unknown: {error}"),
                }
            );
            0
        }
        Command::Serve => unreachable!("serve runs through `serve`"),
    }
}

/// Why `start` or `serve` would not start the server, if anything.
fn start_refusal(paths: &Paths) -> Option<String> {
    match files::check(paths) {
        Ok(ApprovalState::Approved(_)) => {}
        Ok(ApprovalState::Unset) => {
            return Some(
                "server_not_approved: approve a llama-server with asura llamacpp approve PATH."
                    .into(),
            );
        }
        Ok(ApprovalState::Changed { record, current }) => {
            return Some(format!(
                "server_changed: {} was approved as {} and is now {}; run asura llamacpp approve {} again.",
                record.path.display(),
                short(&record.digest_hex()),
                short(&current.digest_hex()),
                record.path.display()
            ));
        }
        Ok(ApprovalState::Missing(record)) => {
            return Some(format!(
                "server_missing: {} is gone.",
                record.path.display()
            ));
        }
        Err(error) => return Some(format!("server_record_unreadable: {error}.")),
    }
    match files::list(paths) {
        Ok(links) if links.iter().any(|l| l.size.is_some()) => None,
        Ok(_) => Some("no_models: add one with asura llamacpp add PATH.".into()),
        Err(error) => Some(format!("models_unreadable: {error}.")),
    }
}

/// The serve step (LC-F3), run by launchd. Every refusal writes one line to
/// the log and returns 0, so launchd does not loop on it (LCD14).
pub(crate) fn serve(runtime: &RuntimeDirectory) -> i32 {
    let paths = Paths::of(runtime);
    let Ok(mut log) = files::open_log(&paths) else {
        return 0;
    };
    let mut note = |text: &str| {
        let _ = writeln!(log, "asura llamacpp serve: {text}");
    };
    let settings = match settings(runtime) {
        Ok(settings) => settings,
        Err(code) => {
            note(&format!("refused: configuration {code}"));
            return 0;
        }
    };
    if settings.server != LlamacppServer::Asura {
        note("refused: mode_not_asura");
        return 0;
    }
    if let Some(reason) = start_refusal(&paths) {
        note(&format!("refused: {reason}"));
        return 0;
    }
    let Ok(Some(approval)) = files::record(&paths) else {
        note("refused: server_not_approved");
        return 0;
    };
    let key = match server_key() {
        Ok(key) => key,
        Err(reason) => {
            note(&format!("refused: key_unavailable ({reason})"));
            return 0;
        }
    };
    let plan = server::ServerPlan {
        binary: approval.path,
        models: paths.models.clone(),
        port: settings.port,
        context_length: settings.context_length,
        slots: settings.slots,
        models_loaded: settings.models_loaded,
        idle_seconds: settings.idle_seconds,
    };
    note(&format!(
        "starting {} on 127.0.0.1:{} (SHA-256 {})",
        plan.binary.display(),
        plan.port,
        short(&files::hex(&approval.sha256))
    ));
    let error = server::exec(&plan, &key[..], &log);
    let mut log = log;
    let _ = writeln!(log, "asura llamacpp serve: refused: exec failed ({error})");
    0
}

/// Reads the server key item, or adds a new one once (the key).
fn server_key() -> Result<impl std::ops::Deref<Target = Vec<u8>>, String> {
    use asura_platform::keychain::{Added, Found, Keychain, Target};
    let keychain = Keychain::open(&Target::Default).map_err(|f| format!("{f:?}"))?;
    let valid =
        |key: &[u8]| key.len() == server::KEY_HEX_BYTES && key.iter().all(u8::is_ascii_hexdigit);
    for _ in 0..2 {
        match keychain.get_server_key().map_err(|f| format!("{f:?}"))? {
            Found::Data(key) if valid(&key) => return Ok(key),
            Found::Data(_) => return Err("invalid item".into()),
            Found::Missing => {
                let key = server::new_key();
                match keychain
                    .add_server_key(&key)
                    .map_err(|f| format!("{f:?}"))?
                {
                    Added::Stored => return Ok(key),
                    // Another start added one first: read it.
                    Added::Duplicate => continue,
                }
            }
        }
    }
    Err("the item could not be read".into())
}

#[cfg(test)]
#[path = "llamacpp_cli/tests.rs"]
mod tests;
