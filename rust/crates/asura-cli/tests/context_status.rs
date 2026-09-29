//! Real service observation subscriptions; fixtures never use the account runtime.
use super::*;
use asura_control::pb;
fn attach(case: &Case) -> Result<Client> {
    let runtime = RuntimeDirectory::scratch(&case.root.join("home"), false)?;
    Ok(Client::attach(&runtime, BUILD, Instant::now() + WAIT)?)
}
fn git(path: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("/usr/bin/git")
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    require(status.success(), "Git fixture setup failed")
}
fn next(
    client: &mut Client,
    request: &mut pb::ObserveContext,
    state: u32,
) -> Result<pb::ContextObservation> {
    let end = Instant::now() + WAIT;
    loop {
        let result = client.observe_context(request.clone())?;
        request.after_subscription = result.subscription_id.clone();
        request.after_revision = result.revision;
        if result.pending == Some(false) && result.git_state == Some(state) {
            return Ok(result);
        }
        require(
            Instant::now() < end,
            &format!("context did not reach state {state}: {result:?}"),
        )?;
    }
}
pub(super) fn observations(case: &mut Case) -> Result<()> {
    let started = case.command(&["service", "start"])?;
    require(started.status.success(), "context fixture service failed")?;
    let end = Instant::now() + WAIT;
    loop {
        if attach(case)?.inspect_installation()?.installation == pb::InstallationState::GraphReady {
            break;
        }
        require(Instant::now() < end, "context installation unavailable")?;
        std::thread::sleep(Duration::from_millis(20));
    }
    let project = case.root.join("project");
    fs::create_dir(&project)?;
    fs::create_dir(project.join("src"))?;
    git(&project, &["init", "-q", "-b", "context-test"])?;
    let registered = attach(case)?.register_project(pb::ProjectRegister {
        request_id: Some(asura_platform::random_id().to_vec()),
        location: Some(project.to_str().unwrap().into()),
    })?;
    require(registered.error.is_none(), "project registration failed")?;
    // Retained status requests must release on actual changes and preserve
    // their cursor on a liveness heartbeat. Mutations use another attachment.
    let mut status_client = attach(case)?;
    let mut status = status_client.observe_service(None)?;
    let stable_by = Instant::now() + WAIT;
    loop {
        let next = status_client.observe_service(Some(status.revision))?;
        if next.pending {
            require(
                next.revision == status.revision,
                "heartbeat advanced service cursor",
            )?;
            break;
        }
        status = next;
        require(
            Instant::now() < stable_by,
            "service subscription did not settle",
        )?;
    }
    let before = status.revision;
    let observer = std::thread::spawn(move || status_client.observe_service(Some(before)));
    let changed = attach(case)?.config("model", Some("ollama:event-test"))?;
    require(changed.error.is_none(), "model fixture config failed")?;
    let changed = observer.join().map_err(|_| "service observer panicked")??;
    let mut observed = changed;
    let mut status_client = attach(case)?;
    let end = Instant::now() + WAIT;
    while observed.configured_model.as_deref() != Some("ollama:event-test") {
        require(Instant::now() < end, "model change was not published")?;
        observed = status_client.observe_service(Some(observed.revision))?;
    }
    require(
        observed.revision > before,
        "model change did not advance service cursor",
    )?;
    let project_id: [u8; 16] = registered.project_id.clone().unwrap().try_into().unwrap();
    let mut queue_client = attach(case)?;
    let queue = queue_client.observe_queue(project_id, None)?;
    require(
        queue.pending == Some(false),
        "initial queue projection missing",
    )?;
    let heartbeat = queue_client.observe_queue(project_id, queue.revision)?;
    require(
        heartbeat.pending == Some(true)
            && heartbeat.entries.is_empty()
            && heartbeat.revision == queue.revision,
        "queue heartbeat replaced or advanced projection",
    )?;
    drop(queue_client);
    drop(status_client);
    let mut query = pb::ObserveContext {
        project_id: registered.project_id.clone(),
        working_directory: Some(project.join("src").to_str().unwrap().into()),
        ..Default::default()
    };
    let mut first = attach(case)?;
    let clean = next(&mut first, &mut query, 2)?;
    require(
        clean.branch.as_deref() == Some("context-test") && clean.unborn == Some(true),
        "wrong branch observation",
    )?;
    let mut second = attach(case)?;
    let shared = second.observe_context(pb::ObserveContext {
        after_subscription: None,
        after_revision: None,
        ..query.clone()
    })?;
    require(
        shared.subscription_id == clean.subscription_id,
        "identical scopes did not share observer",
    )?;
    fs::write(project.join("src/change.txt"), b"signal\n")?;
    let dirty = next(&mut first, &mut query, 3)?;
    require(
        dirty.files_changed == Some(1) && dirty.added == Some(0) && dirty.deleted == Some(0),
        "untracked file count or tracked line totals incorrect",
    )?;
    require(
        dirty.revision > clean.revision,
        "filesystem signal did not advance observation",
    )?;
    // A second scope is independent. A third cannot replace an occupied worker.
    let mut root_view = attach(case)?;
    let mut root_query = pb::ObserveContext {
        working_directory: Some(project.to_str().unwrap().into()),
        after_subscription: None,
        after_revision: None,
        ..query.clone()
    };
    let root_observation = next(&mut root_view, &mut root_query, 3)?;
    require(
        root_observation.subscription_id != dirty.subscription_id,
        "different scopes shared a revision cursor",
    )?;
    fs::create_dir(project.join("other"))?;
    let mut overflow = attach(case)?;
    require(
        overflow
            .observe_context(pb::ObserveContext {
                working_directory: Some(project.join("other").to_str().unwrap().into()),
                after_subscription: None,
                after_revision: None,
                ..query.clone()
            })
            .is_err(),
        "observer capacity was not bounded",
    )?;
    drop(overflow);
    drop(root_view);
    // Dropping one attachment must preserve the other subscriber.
    drop(first);
    fs::remove_file(project.join("src/change.txt"))?;
    fs::remove_dir(project.join("other"))?;
    let again = next(&mut second, &mut query, 2)?;
    require(
        again.subscription_id == clean.subscription_id,
        "disconnect removed another subscriber",
    )?;
    // Lexically outside the registered root must never collect evidence.
    let invalid = second.observe_context(pb::ObserveContext {
        working_directory: Some(case.root.to_str().unwrap().into()),
        after_subscription: None,
        after_revision: None,
        ..query
    });
    require(invalid.is_err(), "outside context accepted")?;
    drop(second);
    // Cleanup stops the service and proves ownership release after observer cancellation.
    Ok(())
}
