//! Thin production terminal client. All observations come from the service.
mod audit;
mod config;
mod context;
mod conversation;
mod editor;
mod events;
mod history;
mod models;
mod observation;
mod project_admin;
mod queue;
mod queue_watch;
mod terminal;
mod ui;
use crossterm::event::{Event, KeyEventKind};
use events::Source;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};
use std::{cell::RefCell, rc::Rc};

struct Output(Rc<RefCell<asura_platform::TerminalOutput>>);
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

pub(crate) fn run(
    resolve: observation::Resolver,
    log_resolve: observation::LogResolver,
) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Asura's TUI requires a terminal; use --help for service commands",
        ));
    }
    terminal::install_panic_hook();
    let mut session = terminal::Session::enter()?;
    let output = Rc::new(RefCell::new(asura_platform::TerminalOutput::default()));
    let mut screen = Terminal::new(CrosstermBackend::new(Output(output.clone())))?;
    let router = events::Router::start()?;
    let worker = observation::Worker::start(resolve, log_resolve, router.notice(Source::Status))?;
    let mut conversation_worker = conversation::Worker::default();
    let mut config_worker = config::Worker::default();
    let mut models_worker = models::Worker::default();
    let mut audit_worker = audit::Worker::default();
    let mut queue_worker = queue::Worker::default();
    let mut project_admin_worker = project_admin::Worker::default();
    let mut context_worker = context::Worker::default();
    let mut queue_observer = queue_watch::Worker::default();
    conversation_worker.set_notice(router.notice(Source::Conversation));
    config_worker.set_notice(router.notice(Source::Config));
    models_worker.set_notice(router.notice(Source::Models));
    audit_worker.set_notice(router.notice(Source::Audit));
    queue_worker.set_notice(router.notice(Source::Queue));
    project_admin_worker.set_notice(router.notice(Source::Conversation));
    context_worker.set_notice(router.notice(Source::Context));
    queue_observer.set_notice(router.notice(Source::QueueWatch));
    let result = (|| {
        let mut app = ui::App::new();
        let mut model = observation::Model::default();
        app.set_view(model.view());
        let mut continuity = observation::Continuity::new();
        let mut dirty = true;
        let mut exit = false;
        let mut settlement_at: Option<Instant> = None;
        let mut output_at: Option<Instant> = None;
        let mut pending_input = std::collections::VecDeque::new();
        while !exit && !session.interrupted() {
            let now = Instant::now();
            // User input and cancel are handled before command results and telemetry.
            if pending_input.is_empty() {
                pending_input.extend(router.input()?);
            }
            let input_deadline = Instant::now() + Duration::from_millis(2);
            while let Some(value) = pending_input.pop_front() {
                let value = match value {
                    events::Input::Event(value) => value,
                    events::Input::PasteReady => {
                        let text = router.take_paste()?;
                        app.handle(Event::Paste(text.into_string()));
                        router.paste_consumed();
                        dirty = true;
                        if Instant::now() >= input_deadline {
                            break;
                        }
                        continue;
                    }
                    events::Input::PasteRejected => {
                        app.reject_paste();
                        dirty = true;
                        if Instant::now() >= input_deadline {
                            break;
                        }
                        continue;
                    }
                };
                if !matches!(value, Event::Key(key) if key.kind == KeyEventKind::Release) {
                    exit = app.handle(value);
                    dirty = true;
                    if exit {
                        break;
                    }
                }
                if Instant::now() >= input_deadline {
                    break;
                }
            }
            if exit {
                break;
            }
            if app.take_cancel_request() {
                conversation_worker.cancel();
            }
            if app.take_audit_cancel() {
                audit_worker.cancel();
            }
            if app.take_models_cancel() {
                models_worker.cancel();
            }
            let settling = settlement_at.is_some_and(|at| now >= at);
            let ready = router.ready(settling);
            let has = |source: Source| ready & source as u8 != 0;
            if (has(Source::Config) || config_worker.next_deadline().is_some_and(|at| now >= at))
                && let Some(outcome) = config_worker.poll()
            {
                app.config_finished(outcome);
                dirty = true;
            }
            if (has(Source::Models) || models_worker.next_deadline().is_some_and(|at| now >= at))
                && let Some(outcome) = models_worker.poll()
            {
                app.models_finished(outcome);
                dirty = true;
            }
            if (has(Source::Audit) || audit_worker.next_deadline().is_some_and(|at| now >= at))
                && let Some(outcome) = audit_worker.poll()
            {
                app.audit_finished(outcome);
                dirty = true;
            }
            if has(Source::Conversation)
                && let Some(update) = conversation_worker.poll()
            {
                app.conversation_finished(update);
                dirty = true;
            }
            if (has(Source::Conversation)
                || project_admin_worker
                    .next_deadline()
                    .is_some_and(|at| now >= at))
                && let Some(update) = project_admin_worker.poll()
            {
                app.project_admin_finished(update);
                dirty = true;
            }
            if (has(Source::Queue) || queue_worker.next_deadline().is_some_and(|at| now >= at))
                && let Some(update) = queue_worker.poll()
            {
                app.queue_finished(update);
                dirty = true;
            }
            if has(Source::QueueWatch)
                && let Some(update) = queue_observer.poll()
            {
                dirty |= app.queue_observed(update);
            }
            if has(Source::Context)
                && let Some(update) = context_worker.poll()
            {
                dirty |= app.context_finished(update);
            }
            if continuity.check(now, std::time::SystemTime::now()) {
                model.retire(now);
                app.set_view(model.view());
                dirty = true;
            }
            if has(Source::Status)
                && let Some(value) = worker.take()
            {
                let changed = model.apply(value);
                if changed {
                    app.set_view(model.view());
                }
                dirty |= changed;
            }
            if model.expire(now) {
                app.set_view(model.view());
                dirty = true;
            }
            dirty |= app.refresh_context(now);
            dirty |= app.refresh_queue_scope(now);
            for (source, running) in [
                (Source::Config, config_worker.running()),
                (Source::Models, models_worker.running()),
                (Source::Audit, audit_worker.running()),
                (
                    Source::Conversation,
                    conversation_worker.running() || project_admin_worker.running(),
                ),
                (Source::Queue, queue_worker.running()),
                (Source::Context, context_worker.running()),
                (Source::Status, worker.running()),
                (Source::QueueWatch, queue_observer.retained()),
            ] {
                if !running {
                    router.settled(source);
                }
            }
            // App produces typed requests; this dispatcher alone starts transport work.
            if let Some((scope, limit)) = app.take_audit_request() {
                if let Err(error) = audit_worker.submit(resolve, scope, limit) {
                    app.audit_finished(Err(error.into()));
                }
                dirty = true;
            }
            if app.take_models_request() {
                if let Err(error) = models_worker.submit(resolve) {
                    app.models_finished(Err(error.into()));
                }
                dirty = true;
            }
            if let Some(request) = app.take_config_request() {
                if let Err(error) = config_worker.submit(resolve, request) {
                    app.config_finished(Err(error.into()));
                }
                dirty = true;
            }
            if !conversation_worker.is_busy()
                && let Some(request) = app.take_conversation_request()
            {
                if let Err(error) = conversation_worker.submit(resolve, request) {
                    app.conversation_finished(conversation::Update {
                        message: Some(error),
                        done: true,
                        ..Default::default()
                    });
                }
                dirty = true;
            }
            if !queue_worker.is_busy()
                && let Some(request) = app.take_queue_request(now)
            {
                if let Err(error) = queue_worker.submit(resolve, request.clone()) {
                    app.queue_finished(queue::Update {
                        request,
                        outcome: Err(error),
                        done: true,
                    });
                }
                dirty = true;
            }
            if !project_admin_worker.is_busy()
                && let Some(request) = app.take_project_admin_request()
            {
                if let Err(error) = project_admin_worker.submit(resolve, request.clone()) {
                    app.project_admin_finished(project_admin::Update {
                        request,
                        outcome: project_admin::Outcome::Definite(error),
                        service_epoch: None,
                        settled: true,
                    });
                }
                dirty = true;
            }
            if context_worker
                .synchronize(resolve, app.context_scope(), now)
                .is_err()
            {
                dirty |= app.context_unavailable();
            }
            // A spawn failure retains the observer's explicit retry deadline;
            // its previous projection expires independently.
            let _ = queue_observer.synchronize(resolve, app.queue_scope(), now);
            output.borrow_mut().pump()?;
            if dirty && !output.borrow().pending() {
                screen.draw(|frame| app.draw(frame))?;
                dirty = false;
            }
            let pending = output.borrow().pending();
            if pending {
                output_at.get_or_insert(now + Duration::from_millis(100));
            } else {
                output_at = None;
            }
            if router.has_settling() {
                if settling || settlement_at.is_none() {
                    settlement_at = Some(now + Duration::from_millis(10));
                }
            } else {
                settlement_at = None;
            }
            let deadline = [
                app.next_deadline(),
                model.next_deadline(),
                config_worker.next_deadline(),
                models_worker.next_deadline(),
                audit_worker.next_deadline(),
                queue_worker.next_deadline(),
                project_admin_worker.next_deadline(),
                context_worker.next_deadline(),
                queue_observer.next_deadline(),
                settlement_at,
                output_at,
            ]
            .into_iter()
            .flatten()
            .min();
            let timeout = if !pending_input.is_empty() {
                Duration::ZERO
            } else {
                deadline.map_or(Duration::from_secs(86400), |at| {
                    at.saturating_duration_since(Instant::now())
                })
            };
            let mut interests = vec![
                asura_platform::PollInterest {
                    fd: router.fd(),
                    read: true,
                    write: false,
                },
                asura_platform::PollInterest {
                    fd: session.signal_fd(),
                    read: true,
                    write: false,
                },
            ];
            if pending {
                interests.push(asura_platform::PollInterest {
                    fd: 1,
                    read: false,
                    write: true,
                });
            }
            let readiness = asura_platform::poll(&interests, timeout)
                .map_err(|e| io::Error::other(e.to_string()))?;
            if readiness.iter().any(|event| event.index == 1) {
                break;
            }
        }
        Ok::<_, io::Error>(())
    })();
    router.cancel();
    context_worker.cancel();
    queue_observer.cancel();
    conversation_worker.disconnect();
    queue_worker.disconnect();
    project_admin_worker.cancel();
    config_worker.cancel();
    models_worker.cancel();
    audit_worker.cancel();
    worker.cancel();
    drop(screen);
    // Terminal ownership settles before any wait on an in-flight client request.
    let restored = session.finish();
    drop(router);
    drop(config_worker);
    drop(models_worker);
    drop(audit_worker);
    drop(context_worker);
    drop(queue_observer);
    let joined = worker.join();
    match (result, restored, joined) {
        (Err(error), Err(cleanup), _) => Err(io::Error::new(
            error.kind(),
            format!("{error}; terminal restore failed: {cleanup}"),
        )),
        (Err(error), _, _) => Err(error),
        (_, Err(error), _) | (_, _, Err(error)) => Err(error),
        _ => Ok(()),
    }
}
