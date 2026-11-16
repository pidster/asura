//! `/policy` in the TUI (security-policy.md, PL3 control and
//! presentation): `/policy`, `/policy admit FILE`, `/policy rollback
//! REVISION` and `/policy cancel`, their replies, and the candidate's
//! result lines when the observed `PolicyStatus.candidate` changes.
//! Rendering and key handling perform no I/O: the policy worker reads the
//! file and sends the request.
use super::*;
use crate::policy_text::{candidate_lines, error_line, finished, policy_line, queued_line};
use crate::tui::policy::{Command, Outcome, Request, Update};
use asura_control::pb;
use asura_control::policy::state;

const USAGE: &str = "Use /policy, /policy admit FILE, /policy rollback REVISION or /policy cancel";

/// What a `/policy` line asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Parsed {
    /// `/policy`: the status panel, from the last observation.
    Show,
    Send(Command),
    /// `/policy cancel`: the shown candidate, once the observation names it.
    CancelShown,
}

/// `/policy` arguments: the rest of the line after `admit` is the file, so
/// a path may hold spaces.
pub(super) fn parse(draft: &str) -> Result<Parsed, &'static str> {
    let rest = draft.trim().strip_prefix("/policy").ok_or(USAGE)?;
    if rest.is_empty() {
        return Ok(Parsed::Show);
    }
    let rest = rest.strip_prefix(char::is_whitespace).ok_or(USAGE)?.trim();
    if rest == "cancel" {
        return Ok(Parsed::CancelShown);
    }
    let (verb, argument) = rest.split_once(char::is_whitespace).ok_or(USAGE)?;
    let argument = argument.trim();
    match verb {
        "admit" if !argument.is_empty() => Ok(Parsed::Send(Command::Admit(argument.into()))),
        "rollback" => match argument.parse::<u64>() {
            Ok(revision) if revision > 0 => Ok(Parsed::Send(Command::Rollback(revision))),
            _ => Err(USAGE),
        },
        _ => Err(USAGE),
    }
}

/// The `/policy` panel: the status line, then the shown candidate.
pub(super) fn panel_text(status: Option<&pb::PolicyStatus>) -> String {
    let mut lines = vec![policy_line(status).unwrap_or_else(|| "policy: not reported yet".into())];
    if let Some(status) = status {
        if let Some(queue) = status.admission_queue.filter(|n| *n > 0) {
            lines.push(format!(
                "policy: {queue} candidate{} waiting",
                if queue == 1 { "" } else { "s" }
            ));
        }
        if let Some(candidate) = &status.candidate {
            lines.extend(candidate_lines(candidate));
        }
    }
    lines.join("\n")
}

/// `/policy` state beside the person log.
#[derive(Default)]
pub(super) struct PolicyUi {
    pub request: Option<Request>,
    pub busy: bool,
    pub cancel: bool,
    /// The candidate this client submitted, whose result opens the panel.
    pub submitted: Option<Vec<u8>>,
    /// The last shown candidate's ID and state.
    pub shown: Option<(Vec<u8>, u32)>,
}

impl App {
    pub(super) fn policy_command(&mut self, draft: String) {
        if self.person.policy_ui.busy {
            self.notice = "Policy request pending; draft retained".into();
            return;
        }
        let command = match parse(&draft) {
            Ok(Parsed::Show) => {
                self.editor = Editor::new();
                self.notice.clear();
                self.result_title = "Policy";
                self.config_result = Some(panel_text(self.person.policy.as_ref()));
                return;
            }
            Ok(Parsed::Send(command)) => command,
            Ok(Parsed::CancelShown) => {
                let waiting = self
                    .person
                    .policy
                    .as_ref()
                    .and_then(|p| p.candidate.as_ref())
                    .filter(|c| !finished(c))
                    .and_then(|c| c.candidate_id.as_deref())
                    .and_then(|id| <[u8; 16]>::try_from(id).ok());
                match waiting {
                    Some(id) => Command::Cancel(id),
                    None => {
                        self.notice = "No policy candidate is waiting; draft retained".into();
                        return;
                    }
                }
            }
            Err(usage) => {
                self.notice = usage.into();
                return;
            }
        };
        self.person.policy_ui.request = Some(Request { command, draft });
        self.person.policy_ui.busy = true;
        self.notice = "Policy request pending · Esc stops waiting".into();
    }
    pub(in super::super) fn take_policy_request(&mut self) -> Option<Request> {
        self.person.policy_ui.request.take()
    }
    pub(in super::super) fn take_policy_cancel(&mut self) -> bool {
        std::mem::take(&mut self.person.policy_ui.cancel)
    }
    /// `/cancel` or Escape while a policy request waits.
    pub(super) fn policy_stop(&mut self) -> bool {
        if !self.person.policy_ui.busy {
            return false;
        }
        self.person.policy_ui.cancel = true;
        self.notice = "Policy request wait stopped; the request may still be applied".into();
        true
    }
    /// The immediate reply: queued, cancel requested, or an error token.
    pub(in super::super) fn policy_finished(&mut self, update: Update) {
        self.person.policy_ui.busy = false;
        let request = update.request;
        match update.outcome {
            Outcome::Reply(reply) => match reply.error.as_deref() {
                Some(token) => self.notice = format!("{}; draft retained", error_line(token)),
                None => {
                    if self.editor.text() == request.draft {
                        self.editor = Editor::new();
                    }
                    self.notice = match request.command {
                        Command::Cancel(_) => format!(
                            "policy: cancel requested for candidate {}",
                            crate::policy_text::short_id(
                                &reply.candidate_id.clone().unwrap_or_default()
                            )
                        ),
                        _ => {
                            self.person.policy_ui.submitted = reply.candidate_id.clone();
                            queued_line(&reply)
                        }
                    };
                }
            },
            Outcome::Definite(error) => {
                self.notice = format!("policy: {}; draft retained", literal(&error, 256));
            }
            Outcome::Unconfirmed(error) => {
                self.notice = format!(
                    "policy: {}; the outcome is unconfirmed; /policy shows the current state",
                    literal(&error, 256)
                );
            }
        }
    }
    /// A new observation: a changed candidate state becomes the notice;
    /// this client's finished candidate opens the panel with every line.
    pub(super) fn policy_observed(&mut self) {
        let submitted = self.person.policy_ui.submitted.clone().unwrap_or_default();
        let Some(candidate) = self
            .person
            .policy
            .clone()
            .and_then(|p| crate::policy_text::shown_candidate(p, &submitted))
        else {
            return;
        };
        let (Some(id), Some(now)) = (candidate.candidate_id.clone(), candidate.state) else {
            return;
        };
        if self.person.policy_ui.shown.as_ref() == Some(&(id.clone(), now)) {
            return;
        }
        let first = self.person.policy_ui.shown.is_none();
        self.person.policy_ui.shown = Some((id.clone(), now));
        let ours = self.person.policy_ui.submitted.as_deref() == Some(id.as_slice());
        // The first observation of a client reports nothing old.
        if first && !ours {
            return;
        }
        let lines = candidate_lines(&candidate);
        self.notice = lines.first().cloned().unwrap_or_default();
        if ours && finished(&candidate) {
            self.person.policy_ui.submitted = None;
            if now != state::ACTIVATED || lines.len() > 1 {
                self.result_title = "Policy";
                self.config_result = Some(lines.join("\n"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_arguments() {
        assert_eq!(parse("/policy"), Ok(Parsed::Show));
        assert_eq!(parse("/policy cancel"), Ok(Parsed::CancelShown));
        assert_eq!(
            parse("/policy admit my policies/user.cedar"),
            Ok(Parsed::Send(Command::Admit(
                "my policies/user.cedar".into()
            )))
        );
        assert_eq!(
            parse("/policy rollback 3"),
            Ok(Parsed::Send(Command::Rollback(3)))
        );
        for bad in [
            "/policyx",
            "/policy admit",
            "/policy rollback 0",
            "/policy rollback x",
            "/policy explain",
            "/policy cancel now",
        ] {
            assert_eq!(parse(bad), Err(USAGE), "{bad}");
        }
    }

    #[test]
    fn panel_lists_status_and_candidate() {
        assert_eq!(panel_text(None), "policy: not reported yet");
        let status = pb::PolicyStatus {
            state: Some(2),
            revision: Some(4),
            installation: Some(1),
            user: Some(1),
            solver: Some(2),
            approvals_waiting: Some(0),
            admission_queue: Some(1),
            candidate: Some(pb::PolicyCandidate {
                candidate_id: Some(vec![0xab; 16]),
                state: Some(state::PROVING),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            panel_text(Some(&status)),
            "policy: revision 4 · installation ok · user ok · workspace 0 · repository 0 · solver: available · 0 approvals waiting\npolicy: 1 candidate waiting\npolicy: proving candidate abababab"
        );
    }
}
