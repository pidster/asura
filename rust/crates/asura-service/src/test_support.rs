//! Worker-layer harness for J1 qualification (feature `test-support` only;
//! never in production builds). It drives real exec_cmd workers and capture
//! workers on one test reactor, as the conversation owner does, without a
//! journal: settlement and install follow the same order (R2).
use crate::capture_worker;
use crate::exec_cmd_worker::{Dispatch, ExecCmdRun, Worker};
use crate::tools::{self, Arguments};
use asura_platform::RuntimeDirectory;
use asura_platform::secrets::{self, JobRedaction, SecretVariable};
use asura_storage::authority::conversation::{
    self as journal, CaptureSettled, CaptureState, Id, ToolResult,
};
use asura_storage::payload::{Page, PageRequest, PayloadStore, read_page};
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
            .admit(installation, operation, 1, 1, job_id)
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
                    call: job_id,
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
                    store.unhold(capture.call);
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
