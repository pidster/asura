//! Worker-layer harnesses (feature `test-support` only; never in production
//! builds). [`run_jobs`] drives real exec_cmd workers and capture workers on
//! one test reactor for J1, without a journal: settlement and install follow
//! the same order (R2). [`JobHarness`] drives the real conversation owner
//! and its job controller for J2.
use crate::capture_worker;
use crate::exec_cmd_worker::{Dispatch, ExecCmdRun, Worker};
use crate::tools::{self, Arguments};
use asura_platform::RuntimeDirectory;
use asura_platform::secrets::{self, JobRedaction, SecretVariable};
use asura_storage::authority::conversation::{
    self as journal, CaptureSettled, CaptureState, Id, ToolResult,
};
use asura_storage::authority::writer;
use asura_storage::payload::{Page, PageRequest, PayloadStore, read_page};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One exec_cmd call for the harness: an already rewritten command and its
/// secret variables (use `secrets::rewrite_exec_cmd_call`).
pub struct Job {
    pub command: String,
    pub secrets: Vec<SecretVariable>,
}
/// What one call settled with, and the `live` stdout pages read while it ran.
pub struct Outcome {
    pub settled: CaptureSettled,
    pub result: ToolResult,
    pub payloads: [Id; 2],
    pub live_pages: Vec<Page>,
    pub elapsed: Duration,
}

struct Running {
    worker: Worker,
    intent: ExecCmdRun,
    started: Instant,
    output: Option<Outcome>,
    capture: Option<capture_worker::Handle>,
    pages: Vec<Page>,
}

/// Runs `jobs` concurrently, each in its own exec_cmd worker and capture
/// worker, in the project at `project`. With `probe_pages`, the stdout page
/// at offset 0 is read on every reactor turn while a call is live.
/// `cancel_after` cancels every call after that long. Returns after every
/// capture worker has installed or been abandoned, or after `limit`.
#[allow(clippy::too_many_arguments)] // A test harness entry; each argument is a case knob.
pub fn run_jobs(
    runtime: &RuntimeDirectory,
    project: &str,
    store: &Arc<PayloadStore>,
    installation: Id,
    jobs: Vec<Job>,
    probe_pages: bool,
    cancel_after: Option<Duration>,
    limit: Duration,
) -> Result<Vec<Outcome>, String> {
    let identity = asura_platform::ProjectIdentity::open(project, &runtime.state_boundary())
        .map_err(|e| format!("project: {e:?}"))?;
    let (reader, wake) = asura_platform::events::wake_pair().map_err(|e| e.to_string())?;
    let rules = secrets::detection().map_err(|_| "rule set".to_string())?;
    let mut running = Vec::new();
    for job in jobs {
        let operation = asura_platform::random_id();
        let job_id = journal::exec_cmd_job_id(operation, 1, 1);
        let victims = store
            .admit(installation, job_id, operation)
            .map_err(|_| "admission: quota".to_string())?;
        let call = tools::Call {
            operation,
            generation: 1,
            ordinal: 1,
            arguments: Arguments::ExecCmd {
                command: job.command.clone(),
                cwd: ".".into(),
            },
        };
        let intent =
            ExecCmdRun::derive(installation, &call).ok_or_else(|| "exec_cmd call".to_string())?;
        let mut worker = Worker::default();
        worker.register(wake.clone());
        let redaction = JobRedaction::new(rules, &job.secrets);
        worker
            .start(Dispatch {
                runtime: runtime.clone(),
                scope: tools::ExecutionScope {
                    project: [1; 16],
                    path: project.into(),
                    device: identity.device,
                    inode: identity.inode,
                    boundary: runtime.state_boundary(),
                },
                call,
                intent: intent.clone(),
                secrets: job.secrets,
                secret_command: None,
                capture: capture_worker::Start {
                    store: store.clone(),
                    job: job_id,
                    victims,
                    redaction,
                    wake: Some(wake.clone()),
                    gate_home: runtime.scratch_home_for_test().map(|p| p.to_path_buf()),
                },
            })
            .map_err(|e| format!("start: {e:?}"))?;
        running.push(Running {
            worker,
            intent,
            started: Instant::now(),
            output: None,
            capture: None,
            pages: Vec::new(),
        });
    }
    let started = Instant::now();
    let mut sequence = 1u64;
    let mut cancelled = false;
    loop {
        let now = Instant::now();
        if now >= started + limit {
            return Err("harness limit reached".into());
        }
        if !cancelled && cancel_after.is_some_and(|after| now >= started + after) {
            cancelled = true;
            for job in &mut running {
                job.worker
                    .cancel(asura_platform::exec_cmd::StopReason::Cancelled);
            }
        }
        for job in &mut running {
            if job.output.is_none() {
                if probe_pages {
                    let page = read_page(
                        store,
                        PageRequest {
                            payload: job.intent.stdout_payload,
                            offset: 0,
                            limit: 8192,
                            scope: None,
                        },
                    );
                    job.pages.push(page);
                }
                if let Some(output) = job.worker.poll(now) {
                    sequence += 1;
                    let s = &output.settled;
                    if s.state == CaptureState::NotStarted {
                        store.release(job.intent.job);
                    } else {
                        store.settle(
                            job.intent.job,
                            s.state,
                            [
                                (s.stdout.stored_bytes, s.stdout.digest),
                                (s.stderr.stored_bytes, s.stderr.digest),
                            ],
                            sequence,
                            output.capture.is_some(),
                        );
                    }
                    if let Some(capture) = &output.capture
                        && s.state != CaptureState::Unconfirmed
                    {
                        capture.install();
                    }
                    job.capture = output.capture;
                    job.output = Some(Outcome {
                        settled: output.settled,
                        result: output.result,
                        payloads: [job.intent.stdout_payload, job.intent.stderr_payload],
                        live_pages: std::mem::take(&mut job.pages),
                        elapsed: job.started.elapsed(),
                    });
                }
            }
            if let Some(capture) = &mut job.capture {
                capture.drain();
                if capture.exited {
                    store.unhold(capture.job);
                    job.capture = None;
                }
            }
        }
        if running
            .iter()
            .all(|job| job.output.is_some() && job.capture.is_none())
        {
            break;
        }
        let mut interests: Vec<asura_platform::PollInterest> = running
            .iter()
            .flat_map(|job| job.worker.interests())
            .collect();
        interests.push(asura_platform::PollInterest {
            fd: std::os::fd::AsRawFd::as_raw_fd(&reader),
            read: true,
            write: false,
        });
        let next = running
            .iter()
            .filter_map(|job| job.worker.next_deadline(now))
            .min()
            .map_or(Duration::from_millis(100), |deadline| {
                deadline.saturating_duration_since(now)
            })
            .min(Duration::from_millis(100));
        let _ = asura_platform::poll(&interests, next);
        let _ = reader.drain();
    }
    Ok(running.into_iter().filter_map(|job| job.output).collect())
}

// ------------------------------------------------------------------ J2 jobs

/// Worker-layer harness for J2 (tool-job-controller.md, J2 validation): the
/// real conversation owner with its job controller, real journal writer,
/// payload store, slot workers, capture workers and guardians, on one test
/// reactor. It writes fixture turns (kinds 21, 10, 14 `run`, 15 and 8) on
/// the owner's serialized ticket and submits model-origin starts to the
/// controller, as J3's exec_cmd switch will. Feature `test-support` only.
pub struct JobHarness {
    owner: crate::conversation::Owner,
    reader: asura_platform::events::WakeReader,
    runtime: RuntimeDirectory,
    audit: Option<crate::audit::Worker>,
    audit_records: Vec<asura_storage::audit::Record>,
    events: Vec<String>,
    /// The longest single owner poll seen (reactor responsiveness).
    pub longest_poll: Duration,
}

/// One open fixture turn: a new conversation with one operation.
pub struct Turn {
    pub project: Id,
    pub conversation: Id,
    pub operation: Id,
    pub generation: u64,
    next: u32,
}

/// A committed `run` kind 14 whose start is not yet submitted.
pub struct PendingStart {
    pub job: Id,
    origin: crate::job_controller::Origin,
    project: Id,
    conversation: Id,
    command: String,
    secrets: Vec<SecretVariable>,
}

/// What the controller reports of one job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobView {
    /// The TJ-S1 state name, for example `Running`.
    pub state: String,
    /// A queued job's block reason name, for example `AgentSlots`.
    pub reason: Option<String>,
    /// A queued job's brief text.
    pub text: Option<String>,
    pub cancel: Option<journal::JobReason>,
    pub outcome: Option<journal::JobOutcome>,
}

/// Escapes a JSON string value (kind 14 canonical arguments).
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl JobHarness {
    /// Starts the owner on `runtime` (a scratch home) and waits until the
    /// installation is initialized, the payload store is ready and the
    /// controller exists. With `audit`, a real audit worker records the
    /// job events (`audit.enabled` must be set in the home's config).
    pub fn start(runtime: &RuntimeDirectory, audit: bool) -> Result<Self, String> {
        let (reader, wake) = asura_platform::events::wake_pair().map_err(|e| e.to_string())?;
        let mut owner = crate::conversation::Owner::new(runtime.clone(), None);
        let audit = if audit {
            let worker = crate::audit::Worker::start(
                runtime.clone(),
                asura_platform::random_id(),
                "asura/job-harness".into(),
                wake.clone(),
            )
            .map_err(|e| format!("audit: {e}"))?;
            owner.set_audit_emitter(worker.emitter());
            Some(worker)
        } else {
            None
        };
        owner.register(wake);
        owner.open();
        let mut harness = Self {
            owner,
            reader,
            runtime: runtime.clone(),
            audit,
            audit_records: Vec::new(),
            events: Vec::new(),
            longest_poll: Duration::ZERO,
        };
        harness.until(Duration::from_secs(60), |h| {
            h.owner
                .fixture_replay()
                .is_some_and(|r| r.binding.is_some())
                && h.owner.fixture_jobs().is_some()
                && h.owner.fixture_unavailable().is_none()
        })?;
        Ok(harness)
    }

    /// One reactor turn: the owner's poll, then a wait on its interests and
    /// the wake for at most 20 ms (harness pacing only).
    pub fn step(&mut self) {
        let now = Instant::now();
        let _ = self.owner.poll(now);
        let spent = now.elapsed();
        self.longest_poll = self.longest_poll.max(spent);
        for event in self.owner.fixture_job_events() {
            self.events.push(format!("{event:?}"));
        }
        if let Some(audit) = &mut self.audit
            && let Some(snapshot) = audit.poll(Instant::now())
        {
            for record in snapshot.records.iter() {
                if !self
                    .audit_records
                    .iter()
                    .any(|r| r.sequence == record.sequence)
                {
                    self.audit_records.push(record.clone());
                }
            }
        }
        let mut interests = self.owner.interests();
        interests.push(asura_platform::PollInterest {
            fd: std::os::fd::AsRawFd::as_raw_fd(&self.reader),
            read: true,
            write: false,
        });
        let now = Instant::now();
        let wait = self
            .owner
            .next_deadline(now)
            .map_or(Duration::from_millis(20), |d| {
                d.saturating_duration_since(now)
            })
            .min(Duration::from_millis(20));
        let _ = asura_platform::poll(&interests, wait);
        let _ = self.reader.drain();
    }

    /// Steps until `done` holds; `limit` is the test's own budget.
    pub fn until(
        &mut self,
        limit: Duration,
        mut done: impl FnMut(&mut Self) -> bool,
    ) -> Result<(), String> {
        let end = Instant::now() + limit;
        loop {
            if done(self) {
                return Ok(());
            }
            if Instant::now() >= end {
                return Err("harness limit reached".into());
            }
            self.step();
        }
    }

    /// Appends one fixture record at the current revision.
    pub fn append(
        &mut self,
        record: journal::Record,
        admission: Option<writer::AdmissionFence>,
    ) -> Result<writer::Reply, writer::Error> {
        let mut pending = Some((record, admission));
        let mut reply = None;
        let waited = self.until(Duration::from_secs(60), |h| {
            if pending.is_some() && h.owner.fixture_idle() {
                let (record, admission) = pending.take().expect("record");
                let revision = h.owner.fixture_replay().map_or(0, |r| r.revision);
                h.owner.fixture_submit(writer::Command::Append {
                    expected_revision: revision,
                    record,
                    admission,
                });
            }
            if pending.is_none() {
                reply = h.owner.fixture_reply();
            }
            reply.is_some()
        });
        if waited.is_err() {
            return Err(writer::Error::Busy);
        }
        reply.expect("reply")
    }

    /// Registers the project directory at `location`.
    pub fn register(&mut self, location: &str) -> Result<Id, String> {
        let reply = self.command(writer::Command::Register {
            request: asura_platform::random_id(),
            location: location.into(),
        })?;
        reply
            .project
            .map(|p| p.registration.project)
            .ok_or_else(|| "no project".into())
    }

    /// One writer command on the owner's serialized ticket.
    pub fn command(&mut self, command: writer::Command) -> Result<writer::Reply, String> {
        let mut pending = Some(command);
        let mut reply = None;
        self.until(Duration::from_secs(60), |h| {
            if pending.is_some() && h.owner.fixture_idle() {
                h.owner.fixture_submit(pending.take().expect("command"));
            }
            if pending.is_none() {
                reply = h.owner.fixture_reply();
            }
            reply.is_some()
        })?;
        reply.expect("reply").map_err(|e| format!("writer: {e:?}"))
    }

    /// Writes kinds 21 and 10 of a new conversation's turn in `project`.
    pub fn begin_turn(&mut self, project: Id) -> Result<Turn, String> {
        let prompt = "job harness";
        let prepared = self
            .command(writer::Command::Prepare {
                project,
                conversation: None,
                expected_generation: 0,
                prompt: prompt.into(),
            })?
            .prepared
            .ok_or("no prepared turn")?;
        let turn = Turn {
            project,
            conversation: asura_platform::random_id(),
            operation: asura_platform::random_id(),
            generation: 1,
            next: 1,
        };
        let accepted = journal::TurnAccepted {
            request: asura_platform::random_id(),
            digest: journal::request_digest(journal::Request::Submit {
                project,
                conversation: None,
                expected_generation: 0,
                prompt,
            })
            .map_err(|e| format!("digest: {e:?}"))?,
            project,
            original_conversation: None,
            expected_generation: 0,
            conversation: turn.conversation,
            generation: turn.generation,
            task: asura_platform::random_id(),
            operation: turn.operation,
            model: "system".into(),
            configuration_digest: prepared.configuration_digest,
            instructions_digest: [1; 32],
            input_digest: [2; 32],
            prior_operations: Vec::new(),
            prompt: prompt.into(),
            event_cursor: 1,
            composition: Default::default(),
            thinking_level: 0,
            thinking_request: 0,
        };
        self.append(
            journal::Record::TurnAccepted(Box::new(accepted)),
            Some(writer::AdmissionFence {
                project,
                configuration_digest: prepared.configuration_digest,
            }),
        )
        .map_err(|e| format!("kind 21: {e:?}"))?;
        self.append(
            journal::Record::StartAuthorized(journal::StartAuthorized {
                operation: turn.operation,
                generation: turn.generation,
                helper: asura_platform::random_id(),
                input_digest: [2; 32],
            }),
            None,
        )
        .map_err(|e| format!("kind 10: {e:?}"))?;
        Ok(turn)
    }

    /// Writes the turn's terminal (kind 8). Jobs outlive it (JR8).
    pub fn end_turn(&mut self, turn: &Turn) -> Result<(), String> {
        self.append(
            journal::Record::TurnTerminal(journal::TurnTerminal {
                operation: turn.operation,
                generation: turn.generation,
                kind: journal::TerminalKind::Failed,
                cause: journal::Cause::ProviderFailure,
                final_cursor: u64::MAX,
                usage_known: false,
                output_tokens: 0,
                text_offset: 0,
                text: String::new(),
            }),
            None,
        )
        .map(|_| ())
        .map_err(|e| format!("kind 8: {e:?}"))
    }

    /// Appends a `run` exec_cmd kind 14 for `raw` (rewritten as the model
    /// path rewrites it). [`Self::start_job`] then submits its model-origin
    /// start.
    pub fn call(&mut self, turn: &mut Turn, raw: &str) -> Result<PendingStart, String> {
        let rewrite = secrets::rewrite_exec_cmd_call(raw, ".")
            .map_err(|r| format!("rewrite: {r}"))?
            .rewritten()
            .ok_or("whole-command fallback is not a harness case")?;
        let ordinal = turn.next;
        turn.next += 1;
        let arguments = format!(
            "{{\"command\":{},\"cwd\":\".\"}}",
            json_string(&rewrite.command)
        );
        let call = journal::ToolCall {
            operation: turn.operation,
            generation: turn.generation,
            ordinal,
            source: journal::ToolCallSource::GeneratedContent,
            name: "exec_cmd".into(),
            name_bytes: 8,
            tool_operation: Some(journal::ToolOperation::ExecCmd),
            decision: journal::ToolDecision::Run,
            fault: None,
            busy: None,
            form: journal::ArgumentForm::Canonical,
            arguments_bytes: arguments.len() as u32,
            arguments,
            number_kinds: rewrite.variables.iter().map(|v| v.kind()).collect(),
            unnumbered: 0,
            rule_set_revision: 1,
            rule_set_digest: [3; 32],
            command_cut: false,
            whole_command: false,
            refusal: None,
        };
        self.append(journal::Record::ToolCall(Box::new(call)), None)
            .map_err(|e| format!("kind 14: {e:?}"))?;
        let origin = crate::job_controller::Origin::Model {
            operation: turn.operation,
            generation: turn.generation,
            ordinal,
        };
        Ok(PendingStart {
            job: origin.job_id(),
            origin,
            project: turn.project,
            conversation: turn.conversation,
            command: rewrite.command,
            secrets: rewrite.variables,
        })
    }

    /// Submits the call's model-origin start to the controller. Returns
    /// the admission as text: `Accepted`, `Existing`, `Conflict` or
    /// `Busy(..)`.
    pub fn start_job(&mut self, pending: PendingStart) -> Result<String, String> {
        let registration = self
            .owner
            .fixture_replay()
            .and_then(|r| r.projects.get(&pending.project))
            .map(|p| p.registration.clone())
            .ok_or("project not resident")?;
        let rules = secrets::detection().map_err(|_| "rule set")?;
        let mut digest = Sha256::new();
        digest.update(pending.command.as_bytes());
        digest.update([0]);
        digest.update(b".");
        let request = crate::job_controller::StartRequest {
            origin: pending.origin,
            agent: crate::job_controller::AgentKey {
                conversation: pending.conversation,
                agent: 0,
            },
            scope: tools::ExecutionScope {
                project: pending.project,
                path: registration.location.clone(),
                device: registration.device,
                inode: registration.inode,
                boundary: self.runtime.state_boundary(),
            },
            profile: crate::job_controller::Profile::Model1,
            redaction: JobRedaction::new(rules, &pending.secrets),
            command: pending.command,
            cwd: ".".into(),
            secrets: pending.secrets,
            secret_command: None,
            duplicate_digest: digest.finalize().into(),
        };
        let admission = self
            .owner
            .fixture_jobs()
            .ok_or("no controller")?
            .submit(request, Instant::now());
        Ok(format!("{admission:?}"))
    }

    /// `call` then `start`: the job ID and the admission text.
    pub fn submit(&mut self, turn: &mut Turn, raw: &str) -> Result<(Id, String), String> {
        let pending = self.call(turn, raw)?;
        let job = pending.job;
        Ok((job, self.start_job(pending)?))
    }

    /// Writes the call's kind 15 (handle delivery) after its kind 26
    /// committed (JR4).
    pub fn resolve(&mut self, turn: &Turn, job: Id) -> Result<(), String> {
        self.until(Duration::from_secs(60), |h| h.intent_committed(job))?;
        let ordinal = (1..turn.next)
            .find(|ordinal| {
                journal::job_id(&journal::JobOrigin::Model {
                    operation: turn.operation,
                    generation: turn.generation,
                    ordinal: *ordinal,
                }) == job
            })
            .ok_or("not a call of this turn")?;
        self.append(
            journal::Record::ToolResult(ToolResult {
                operation: turn.operation,
                generation: turn.generation,
                ordinal,
                status: 1,
                text: "job started".into(),
                truncated: false,
                next_offset: None,
            }),
            None,
        )
        .map(|_| ())
        .map_err(|e| format!("kind 15: {e:?}"))
    }

    /// `submit` and `resolve`: the job's kind 26 and the call's kind 15 are
    /// committed. Fails unless the start was accepted.
    pub fn launch(&mut self, turn: &mut Turn, raw: &str) -> Result<Id, String> {
        let (job, admission) = self.submit(turn, raw)?;
        if !admission.starts_with("Accepted") {
            return Err(format!("admission: {admission}"));
        }
        self.resolve(turn, job)?;
        Ok(job)
    }

    /// Kind 26 of `job` is committed: replay holds it unsettled, or the
    /// controller has settled it.
    pub fn intent_committed(&mut self, job: Id) -> bool {
        self.owner
            .fixture_replay()
            .is_some_and(|r| r.jobs.contains_key(&job))
            || self.view(job).is_some_and(|v| v.outcome.is_some())
    }

    /// A cancel with the reason's requester (and `kill` for `person_kill`).
    pub fn cancel(&mut self, job: Id, reason: journal::JobReason) -> String {
        use crate::job_controller::Requester;
        let by = match reason.requester() {
            Some(journal::JobRequester::Model) => Requester::Model,
            Some(journal::JobRequester::Client) => Requester::Client,
            Some(journal::JobRequester::Person) => Requester::Person,
            _ => Requester::Service,
        };
        let kill = reason == journal::JobReason::PersonKill;
        self.owner
            .fixture_jobs()
            .map_or("NoController".into(), |c| {
                format!("{:?}", c.cancel(job, by, reason, kill))
            })
    }

    pub fn view(&mut self, job: Id) -> Option<JobView> {
        let status = self.owner.fixture_jobs()?.status(job)?;
        Some(JobView {
            state: format!("{:?}", status.state),
            reason: status.reason.map(|r| format!("{r:?}")),
            text: status.text,
            cancel: status.cancel,
            outcome: status.outcome,
        })
    }

    /// The controller events seen so far, as text, oldest first.
    pub fn events(&self) -> &[String] {
        &self.events
    }
    /// The audit records the real emitter wrote so far.
    pub fn audit_events(&self) -> Vec<asura_storage::audit::Event> {
        self.audit_records.iter().map(|r| r.event.clone()).collect()
    }
    /// An identity recheck (tool call or turn admission) found `project`
    /// replaced (J2-E10).
    pub fn identity_changed(&mut self, project: Id) {
        self.owner.fixture_identity_changed(project);
    }
    /// The owner's storage-unavailable reason, if any.
    pub fn unavailable(&self) -> Option<&'static str> {
        self.owner.fixture_unavailable()
    }
    /// Fault injection: closes the journal writer under the owner, so the
    /// next append fails and the owner becomes storage-unavailable.
    pub fn close_writer(&mut self) {
        self.owner.fixture_writer_close();
    }
    /// The writer's journal status.
    pub fn journal_status(&mut self) -> Result<writer::JournalStatus, String> {
        self.command(writer::Command::JournalStatus)?
            .status
            .ok_or_else(|| "no status".into())
    }
    /// Sets the journal quota (SJ-14) through the writer.
    pub fn set_journal_quota(&mut self, bytes: u64) -> Result<(), String> {
        self.command(writer::Command::SetQuota { bytes })
            .map(|_| ())
    }
    /// The replay's reserved bytes (the ledger and the active operation).
    pub fn reserved_bytes(&self) -> usize {
        self.owner.fixture_replay().map_or(0, |r| r.reserved_bytes)
    }
    /// Shutdown (AE-06): the owner closes admission and stops live jobs.
    pub fn shutdown(&mut self) {
        self.owner.shutdown();
    }
    /// The owner settled after shutdown: jobs settled or the 2 s bound
    /// passed, and the writer closed.
    pub fn settled(&mut self) -> bool {
        self.owner.settled()
    }
}
