//! `asura policy …` (security-policy.md, PL3 control and presentation):
//! status, admit, rollback and cancel. Presentation and exit codes only;
//! the service owns admission and its results. Exit status: 0 activated
//! (or the request answered, for `status` and `cancel`), 1 refused or
//! cancelled, 2 usage, 3 service absent or unavailable.
use super::{BUILD, Client, ClientError, RuntimeResolver, classify, ctrl_c};
use crate::policy_text::{
    candidate_lines, error_line, finished, full_id, parse_id, policy_line, queued_line, read_text,
};
use asura_client::PolicyRequest;
use asura_control::pb;
use asura_control::policy::{SCOPE_USER, state};
use serde_json::{Value, json};

pub(super) const USAGE: &str = "  asura policy status [--json]\n  asura policy admit --scope user FILE\n  asura policy rollback --scope user REVISION\n  asura policy cancel CANDIDATE";

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command {
    Status { json: bool },
    Admit(std::path::PathBuf),
    Rollback(u64),
    Cancel([u8; 16]),
}

/// The words after `policy`.
pub(super) fn parse(words: &[&str]) -> Option<Command> {
    Some(match words {
        ["status"] => Command::Status { json: false },
        ["status", "--json"] => Command::Status { json: true },
        ["admit", "--scope", "user", file] if !file.is_empty() && !file.starts_with("--") => {
            Command::Admit((*file).into())
        }
        ["rollback", "--scope", "user", revision] => {
            let revision: u64 = revision.parse().ok()?;
            if revision == 0 {
                return None;
            }
            Command::Rollback(revision)
        }
        ["cancel", candidate] => Command::Cancel(parse_id(candidate)?),
        _ => return None,
    })
}

fn candidate_json(c: &pb::PolicyCandidate) -> Value {
    json!({
        "candidate_id": c.candidate_id.as_deref().map(full_id),
        "state": match c.state.unwrap_or(0) {
            state::QUEUED => "queued",
            state::PROVING => "proving",
            state::ACTIVATED => "activated",
            state::REJECTED => "rejected",
            state::CANCELLED => "cancelled",
            _ => "unknown",
        },
        "position": c.position,
        "reason": c.reason,
        "details": c.details,
        "counterexamples": c.counterexamples,
        "revision": c.revision,
        "widening": c.widening,
        "widening_examples": c.widening_examples,
        "queries": c.queries,
        "proof_ms": c.proof_ms,
    })
}

/// `asura policy status --json`: the observed facts, schema version 1.
pub(super) fn status_json(status: Option<&pb::PolicyStatus>) -> Value {
    let Some(s) = status else {
        return json!({"schema_version": 1, "policy": null});
    };
    json!({"schema_version": 1, "policy": {
        "state": match s.state { Some(1) => "starting", Some(2) => "active", _ => "unavailable" },
        "revision": s.revision,
        "installation": s.installation,
        "user": s.user,
        "workspace_sets": s.workspace_sets,
        "repository_sets": s.repository_sets,
        "repository_changed": s.repository_changed,
        "solver": match s.solver { Some(2) => "available", Some(3) => "unavailable", _ => "none" },
        "approvals_waiting": s.approvals_waiting,
        "admission_queue": s.admission_queue,
        "candidate": s.candidate.as_ref().map(candidate_json),
    }})
}

/// The exit status of a finished candidate.
pub(super) fn result_code(candidate: &pb::PolicyCandidate) -> i32 {
    if candidate.state == Some(state::ACTIVATED) {
        0
    } else {
        1
    }
}

pub(super) fn run(resolve: RuntimeResolver, command: Command) -> i32 {
    let text = match &command {
        Command::Admit(path) => match read_text(path) {
            Ok(text) => Some(text),
            Err(error) => {
                eprintln!("policy: {error}");
                return 2;
            }
        },
        _ => None,
    };
    let cancel = ctrl_c();
    let attach = || -> Result<Client, ClientError> {
        let runtime = resolve(false).map_err(ClientError::from)?;
        let mut client = Client::attach(&runtime, BUILD, &|| cancel())?;
        client.set_cancel(cancel.clone());
        Ok(client)
    };
    let mut client = match attach() {
        Ok(client) => client,
        Err(error) => return failed(&error),
    };
    let request_id = Some(asura_platform::random_id().to_vec());
    let request = match command {
        Command::Status { json } => {
            return match client.observe_service(None) {
                Ok(update) => {
                    let policy = update.policy.as_ref();
                    if json {
                        println!("{}", status_json(policy));
                    } else {
                        match policy_line(policy) {
                            Some(line) => println!("{line}"),
                            None => println!("policy: not reported"),
                        }
                        if let Some(candidate) = policy.and_then(|p| p.candidate.as_ref()) {
                            for line in candidate_lines(candidate) {
                                println!("{line}");
                            }
                        }
                    }
                    0
                }
                Err(error) => failed(&error),
            };
        }
        Command::Admit(_) => PolicyRequest::Submit(pb::PolicyCandidateSubmit {
            request_id,
            scope: Some(SCOPE_USER),
            text,
        }),
        Command::Rollback(revision) => PolicyRequest::Rollback(pb::PolicyRollback {
            request_id,
            scope: Some(SCOPE_USER),
            revision: Some(revision),
        }),
        Command::Cancel(candidate) => PolicyRequest::Cancel(pb::PolicyCancel {
            request_id,
            candidate_id: Some(candidate.to_vec()),
        }),
    };
    let waits = !matches!(request, PolicyRequest::Cancel(_));
    let reply = match client.policy(request, &|| false) {
        Ok(reply) => reply,
        Err(error) => return failed(&error),
    };
    if let Some(token) = reply.error.as_deref() {
        println!("{}", error_line(token));
        return 1;
    }
    let Some(candidate) = reply.candidate_id.clone() else {
        return 3;
    };
    if !waits {
        println!(
            "policy: cancel requested for candidate {}",
            crate::policy_text::short_id(&candidate)
        );
        return 0;
    }
    println!("{}", queued_line(&reply));
    // The result arrives in the observed status; no time bound. Ctrl-C ends
    // the wait and the candidate continues.
    let mut last_state = None;
    let mut after = None;
    let mut seen = false;
    loop {
        let update = match client.observe_service(after) {
            Ok(update) => update,
            Err(error) => return failed(&error),
        };
        after = Some(update.revision);
        let shown = update
            .policy
            .and_then(|p| crate::policy_text::shown_candidate(p, &candidate));
        match shown {
            Some(shown) if shown.candidate_id.as_deref() == Some(candidate.as_slice()) => {
                seen = true;
                if shown.state != last_state && shown.state != Some(state::QUEUED) {
                    last_state = shown.state;
                    for line in candidate_lines(&shown) {
                        println!("{line}");
                    }
                }
                if finished(&shown) {
                    return result_code(&shown);
                }
            }
            Some(_) if seen => {
                println!(
                    "policy: the candidate's result is no longer shown; run asura policy status"
                );
                return 1;
            }
            _ => {}
        }
    }
}

fn failed(error: &ClientError) -> i32 {
    if asura_client::is_absent(error) {
        eprintln!("policy: the service is not running");
        return 3;
    }
    let (code, _, reason) = classify(error);
    eprintln!("policy: {reason}");
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_arguments() {
        assert_eq!(parse(&["status"]), Some(Command::Status { json: false }));
        assert_eq!(
            parse(&["status", "--json"]),
            Some(Command::Status { json: true })
        );
        assert_eq!(
            parse(&["admit", "--scope", "user", "p.cedar"]),
            Some(Command::Admit("p.cedar".into()))
        );
        assert_eq!(
            parse(&["rollback", "--scope", "user", "3"]),
            Some(Command::Rollback(3))
        );
        let id = "0".repeat(31) + "1";
        assert!(matches!(parse(&["cancel", &id]), Some(Command::Cancel(_))));
        for bad in [
            &["admit", "p.cedar"][..],
            &["admit", "--scope", "workspace", "p.cedar"],
            &["admit", "--scope", "user", "--json"],
            &["rollback", "--scope", "user", "0"],
            &["rollback", "--scope", "user", "x"],
            &["cancel", "abc"],
            &["status", "--yaml"],
            &[],
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn json_and_codes() {
        assert_eq!(status_json(None)["policy"], Value::Null);
        let status = pb::PolicyStatus {
            state: Some(2),
            revision: Some(4),
            solver: Some(2),
            candidate: Some(pb::PolicyCandidate {
                candidate_id: Some(vec![1; 16]),
                state: Some(state::REJECTED),
                reason: Some("proof_failed".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let value = status_json(Some(&status));
        assert_eq!(value["policy"]["solver"], "available");
        assert_eq!(value["policy"]["candidate"]["state"], "rejected");
        assert_eq!(value["policy"]["candidate"]["reason"], "proof_failed");
        assert_eq!(result_code(status.candidate.as_ref().unwrap()), 1);
        assert_eq!(
            result_code(&pb::PolicyCandidate {
                state: Some(state::ACTIVATED),
                ..Default::default()
            }),
            0
        );
    }
}
