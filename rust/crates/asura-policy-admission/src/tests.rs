//! Unit tests (security-policy.md, PL3 validation map: PL-U11 and the
//! adapter unit cases) with an in-memory scripted solver pipe.
use super::*;
use crate::adapter::{Adapter, Reader, Shape, block_on};
use cedar_policy_symcc::solver::{Decision, DecisionWithModel, Solver};
use std::collections::VecDeque;
use std::sync::atomic::Ordering;

const INSTALLATION: &str = include_str!("../../asura-policy/policies/installation.cedar");
const USER: &str = include_str!("../../asura-policy/policies/user-default.cedar");

/// One scripted reply: standard error, then standard output, then the end.
struct Reply {
    stderr: Vec<u8>,
    stdout: Vec<u8>,
    end: Option<PipeEnd>,
}
fn out(stdout: &str) -> Reply {
    Reply {
        stderr: Vec::new(),
        stdout: stdout.as_bytes().to_vec(),
        end: None,
    }
}
/// A pipe that answers each exchange from a script, in 64 KiB chunks, or
/// `unsat` to every exchange when the script is empty and `always` is set.
#[derive(Default)]
struct Script {
    replies: VecDeque<Reply>,
    always_unsat: bool,
    inputs: Vec<Vec<u8>>,
}
impl SolverPipe for Script {
    fn exchange(
        &mut self,
        input: &[u8],
        output: &mut dyn FnMut(PipeStream, &[u8]) -> PipeFlow,
    ) -> Result<(), PipeEnd> {
        self.inputs.push(input.to_vec());
        let reply = match self.replies.pop_front() {
            Some(r) => r,
            None if self.always_unsat => out("unsat\n"),
            None => return Err(PipeEnd::Closed),
        };
        let mut flow = PipeFlow::More;
        for chunk in reply.stderr.chunks(65536) {
            flow = output(PipeStream::Stderr, chunk);
        }
        for chunk in reply.stdout.chunks(65536) {
            flow = output(PipeStream::Stdout, chunk);
            if flow == PipeFlow::Fault {
                return Err(PipeEnd::Fault);
            }
        }
        if let Some(end) = reply.end {
            return Err(end);
        }
        match flow {
            PipeFlow::Answered => Ok(()),
            PipeFlow::Fault => Err(PipeEnd::Fault),
            PipeFlow::More => Err(PipeEnd::Closed),
        }
    }
}

fn script(replies: Vec<Reply>) -> Script {
    Script {
        replies: replies.into(),
        ..Script::default()
    }
}

#[test]
fn answer_lines_map_to_decisions() {
    for (text, want) in [
        ("sat\n", Some(Decision::Sat)),
        ("unsat\n", Some(Decision::Unsat)),
        ("unknown\n", Some(Decision::Unknown)),
        ("(error \"Parse Error: x\")\n", None),
        ("garbage\n", None),
    ] {
        let mut pipe = script(vec![out(text)]);
        let mut adapter = Adapter::new(&mut pipe);
        let got = block_on(adapter.check_sat()).expect("ready");
        assert_eq!(got.ok(), want, "{text}");
        if want.is_none() {
            assert_eq!(adapter.cause, Some(SolverCause::Protocol));
            // After a fault no further exchange runs.
            assert!(block_on(adapter.check_sat()).expect("ready").is_err());
            assert_eq!(adapter.exchanges, 1);
        }
    }
}

#[test]
fn a_query_and_its_check_sat_go_in_one_exchange() {
    let mut pipe = script(vec![out("unsat\n")]);
    let mut adapter = Adapter::new(&mut pipe);
    let write = tokio::io::AsyncWriteExt::write_all(adapter.smtlib_input(), b"(assert false)\n");
    block_on(write).expect("ready").expect("buffered");
    assert_eq!(
        block_on(adapter.check_sat()).expect("ready").ok(),
        Some(Decision::Unsat)
    );
    drop(adapter);
    assert_eq!(pipe.inputs, vec![b"(assert false)\n(check-sat)\n".to_vec()]);
}

#[test]
fn a_model_is_read_up_to_its_closing_line() {
    let mut pipe = script(vec![out("sat\n"), out("(\n(define-fun x () Int 1)\n)\n")]);
    let mut adapter = Adapter::new(&mut pipe);
    match block_on(adapter.check_sat_with_model()).expect("ready") {
        Ok(DecisionWithModel::Sat { model }) => {
            assert_eq!(model, "(\n(define-fun x () Int 1)\n)\n")
        }
        other => panic!("{other:?}"),
    }
    drop(adapter);
    assert_eq!(pipe.inputs[1], b"(get-model)\n");
}

/// SA-T4 (unit): a model line over 1 MiB stops the reading at once.
#[test]
fn a_model_over_one_mib_is_an_output_fault() {
    let mut huge = String::from("(\n");
    huge.push_str(&"x".repeat(MAX_MODEL_BYTES + 1));
    huge.push('\n');
    let mut pipe = script(vec![out("sat\n"), out(&huge)]);
    let mut adapter = Adapter::new(&mut pipe);
    assert!(
        block_on(adapter.check_sat_with_model())
            .expect("ready")
            .is_err()
    );
    assert_eq!(adapter.cause, Some(SolverCause::OutputBound));
    // Many short lines past 1 MiB in all are the same fault.
    let mut reader = Reader::new(Shape::Model);
    assert_eq!(reader.feed(b"(\n"), PipeFlow::More);
    let line = "y".repeat(1023) + "\n";
    let mut flow = PipeFlow::More;
    for _ in 0..1100 {
        flow = reader.feed(line.as_bytes());
        if flow == PipeFlow::Fault {
            break;
        }
    }
    assert_eq!(flow, PipeFlow::Fault);
    assert_eq!(reader.fault, Some(SolverCause::OutputBound));
}

/// SA-T3 (unit): 1 MiB of standard error, then the answer: the first
/// 64 KiB is kept and the query completes.
#[test]
fn standard_error_is_kept_to_64_kib() {
    let mut pipe = script(vec![Reply {
        stderr: vec![b'e'; 1 << 20],
        stdout: b"unsat\n".to_vec(),
        end: None,
    }]);
    let mut adapter = Adapter::new(&mut pipe);
    assert_eq!(
        block_on(adapter.check_sat()).expect("ready").ok(),
        Some(Decision::Unsat)
    );
    assert_eq!(adapter.stderr.len(), MAX_STDERR_KEPT);
    assert_eq!(adapter.stderr_dropped, (1 << 20) - MAX_STDERR_KEPT as u64);
}

/// SA-T2 (unit): output after the answer is drained and refuses.
#[test]
fn extra_output_is_a_protocol_fault() {
    let mut pipe = script(vec![out("unsat\nextra line\n")]);
    let mut adapter = Adapter::new(&mut pipe);
    assert!(block_on(adapter.check_sat()).expect("ready").is_err());
    assert_eq!(adapter.cause, Some(SolverCause::Protocol));
    let mut reader = Reader::new(Shape::Line);
    assert_eq!(reader.feed(b"garbage\nmore\n"), PipeFlow::Answered);
    assert_eq!(reader.extra, 5);
}

#[test]
fn pipe_ends_keep_their_cause() {
    for (end, cause) in [
        (PipeEnd::Closed, SolverCause::Closed),
        (PipeEnd::Cancelled, SolverCause::Cancelled),
        (PipeEnd::MemoryBound, SolverCause::MemoryBound),
        (PipeEnd::Io, SolverCause::Io),
    ] {
        let mut pipe = script(vec![Reply {
            stderr: Vec::new(),
            stdout: Vec::new(),
            end: Some(end),
        }]);
        let mut adapter = Adapter::new(&mut pipe);
        assert!(block_on(adapter.check_sat()).expect("ready").is_err());
        assert_eq!(adapter.cause, Some(cause));
    }
}

/// SA-T6 (unit): a query over 64 MiB fails in the buffer; nothing is
/// exchanged.
#[test]
fn a_query_over_64_mib_is_refused_before_any_exchange() {
    let mut pipe = script(vec![out("unsat\n")]);
    let mut adapter = Adapter::new(&mut pipe);
    let chunk = vec![b' '; 1 << 20];
    let mut failed = false;
    for _ in 0..=64 {
        let write = tokio::io::AsyncWriteExt::write_all(adapter.smtlib_input(), &chunk);
        if block_on(write).expect("ready").is_err() {
            failed = true;
            break;
        }
    }
    assert!(failed);
    assert!(adapter.overflowed());
    assert!(block_on(adapter.check_sat()).expect("ready").is_err());
    assert_eq!(adapter.cause, Some(SolverCause::InputBound));
    assert_eq!(adapter.exchanges, 0);
    drop(adapter);
    assert!(pipe.inputs.is_empty());
}

#[test]
fn a_pending_future_is_not_ready() {
    assert!(block_on(std::future::pending::<()>()).is_none());
    assert_eq!(block_on(std::future::ready(3)), Some(3));
}

fn baseline() -> Baseline<'static> {
    Baseline {
        installation: INSTALLATION,
        user_default: USER,
        previous: Some(USER),
    }
}
fn admit_user(text: &str, solver: Option<&mut dyn SolverPipe>) -> Admission {
    admit(
        &Candidate {
            scope: Scope::User,
            text,
        },
        &baseline(),
        solver,
        &NoLiteralCheck,
        &AtomicBool::new(false),
    )
}
fn reason(admission: &Admission) -> Option<Reason> {
    match &admission.outcome {
        Outcome::Ready => None,
        Outcome::Rejected { reason, .. } => Some(*reason),
    }
}
fn details(admission: &Admission) -> Vec<String> {
    match &admission.outcome {
        Outcome::Ready => Vec::new(),
        Outcome::Rejected { details, .. } => details.clone(),
    }
}

/// Every query of the default user set, with a solver that answers
/// `unsat`, holds: the flow reaches Ready, runs a fixed query list and
/// gives a stable digest.
#[test]
fn all_unsat_admits_the_default_user_set() {
    let mut pipe = Script {
        always_unsat: true,
        ..Script::default()
    };
    let first = admit_user(USER, Some(&mut pipe));
    assert_eq!(first.outcome, Outcome::Ready, "{first:?}");
    assert!(!first.narrows);
    assert!(
        first
            .queries
            .iter()
            .all(|q| q.outcome == QueryOutcome::Holds)
    );
    for id in ["SP-AQ9", "compile-a", "compile-b", "SP-AQ8"] {
        assert!(first.queries.iter().any(|q| q.id == id), "{id}");
    }
    let mut again = Script {
        always_unsat: true,
        ..Script::default()
    };
    let second = admit_user(USER, Some(&mut again));
    assert_eq!(first.digest, second.digest);
    assert_eq!(first.policy_ids, second.policy_ids);
    assert!(first.policy_ids.contains(&"model1.read".to_owned()));
}

#[test]
fn no_solver_refuses_solver_failed_unavailable() {
    let a = admit_user(USER, None);
    match a.outcome {
        Outcome::Rejected { reason, cause, .. } => {
            assert_eq!(reason, Reason::SolverFailed);
            assert_eq!(cause, Some(SolverCause::Unavailable));
        }
        Outcome::Ready => panic!("ready without a solver"),
    }
}

#[test]
fn a_cancel_refuses_cancelled() {
    let mut pipe = Script {
        always_unsat: true,
        ..Script::default()
    };
    let cancel = AtomicBool::new(false);
    cancel.store(true, Ordering::Release);
    let a = admit(
        &Candidate {
            scope: Scope::User,
            text: USER,
        },
        &baseline(),
        Some(&mut pipe),
        &NoLiteralCheck,
        &cancel,
    );
    assert_eq!(reason(&a), Some(Reason::Cancelled));
}

/// Steps 1 to 3 name their refusal.
#[test]
fn bounds_parse_and_validation_refuse() {
    assert_eq!(
        reason(&admit_user(
            &" ".repeat(asura_policy::MAX_SCOPE_TEXT_BYTES + 1),
            None
        )),
        Some(Reason::CandidateTooLarge)
    );
    assert_eq!(
        reason(&admit_user("permit (", None)),
        Some(Reason::CandidateParse)
    );
    assert_eq!(
        reason(&admit_user(
            "@id(\"a\") @reason(\"r\") @envelope(\"x\") forbid (principal, action, resource);",
            None
        )),
        Some(Reason::CandidateParse)
    );
    assert_eq!(
        reason(&admit_user(
            "@id(\"a\") @reason(\"r\") forbid (principal, action, resource) when { principal.nope };",
            None
        )),
        Some(Reason::CandidateInvalid)
    );
}

const READ_INTENT: [&str; 3] = [
    "context has intent && context.intent.name == \"x\"",
    "context has intent && context[\"intent\"].name == \"x\"",
    "context has intent",
];

/// PL-U11: the tighten-only lint finds every spelling of a read, refuses
/// an unannotated read, an annotation without a read and an annotated
/// permit.
#[test]
fn pl_u11_tighten_only_lint() {
    for condition in READ_INTENT {
        let unannotated = format!(
            "@id(\"a\") @reason(\"r\") forbid (principal, action == Asura::Action::\"process.exec\", resource) when {{ {condition} }};"
        );
        let a = admit_user(&unannotated, None);
        assert_eq!(reason(&a), Some(Reason::CandidateLint), "{condition}");
        assert!(details(&a)[0].contains("user:a: reads context.intent"));
        let permit = format!(
            "@id(\"a\") @reason(\"r\") @intent(\"true\") permit (principal, action == Asura::Action::\"process.exec\", resource) when {{ {condition} }};"
        );
        let a = admit_user(&permit, None);
        assert_eq!(reason(&a), Some(Reason::CandidateLint));
        assert!(details(&a).iter().any(|d| d.contains("must be a forbid")));
    }
    let stray = "@id(\"a\") @reason(\"r\") @intent(\"true\") forbid (principal, action == Asura::Action::\"process.exec\", resource);";
    let a = admit_user(stray, None);
    assert_eq!(reason(&a), Some(Reason::CandidateLint));
    assert!(details(&a)[0].contains("does not read context.intent"));
    let risk = "@id(\"a\") @reason(\"r\") forbid (principal, action == Asura::Action::\"process.exec\", resource) when { context has risk };";
    assert!(details(&admit_user(risk, None))[0].contains("context.risk without @classifier"));
    // A correct tighten-only forbid passes the lint (and then needs a solver).
    let ok = "@id(\"a\") @reason(\"r\") @intent(\"true\") forbid (principal, action == Asura::Action::\"process.exec\", resource) when { context has intent };";
    assert_eq!(reason(&admit_user(ok, None)), Some(Reason::SolverFailed));
}

/// PL-U11: forbid-only lower scopes, and path literals.
#[test]
fn pl_u11_forbid_only_and_literals() {
    let repo = "@id(\"open\") @reason(\"r\") permit (principal, action, resource);";
    let a = admit(
        &Candidate {
            scope: Scope::Repository,
            text: repo,
        },
        &baseline(),
        None,
        &NoLiteralCheck,
        &AtomicBool::new(false),
    );
    assert_eq!(reason(&a), Some(Reason::CandidateLint));
    assert!(details(&a)[0].contains("repository:open: a repository policy may only forbid"));
    let alias = "@id(\"t\") @reason(\"r\") forbid (principal, action, resource == Asura::Path::\"/tmp/x\");";
    let a = admit_user(alias, None);
    assert_eq!(reason(&a), Some(Reason::CandidateLint));
    assert!(details(&a)[0].contains("not canonical: /tmp/x"));
    struct Respell;
    impl LiteralCheck for Respell {
        fn check(&self, literal: &str) -> LiteralResult {
            if literal == "/Users/P/X" {
                LiteralResult::Respell("/Users/p/x".into())
            } else {
                LiteralResult::Canonical
            }
        }
    }
    let spelled = "@id(\"t\") @reason(\"r\") forbid (principal, action, resource) when { resource in Asura::Path::\"/Users/P/X\" };";
    let a = admit(
        &Candidate {
            scope: Scope::User,
            text: spelled,
        },
        &baseline(),
        None,
        &Respell,
        &AtomicBool::new(false),
    );
    assert_eq!(reason(&a), Some(Reason::CandidateLint));
    assert!(details(&a)[0].contains("is spelled /Users/p/x on this Mac"));
}

/// A lint refusal with a solver shows a counterexample when the proof
/// finds one: here the scripted solver answers `sat` with an empty model,
/// which SymCC cannot concretize, so the refusal stays a lint refusal.
#[test]
fn a_lint_refusal_stays_a_lint_refusal_when_no_counterexample_is_found() {
    let mut pipe = script(vec![out("garbage\n")]);
    let repo = "@id(\"open\") @reason(\"r\") permit (principal, action, resource);";
    let a = admit(
        &Candidate {
            scope: Scope::Repository,
            text: repo,
        },
        &baseline(),
        Some(&mut pipe),
        &NoLiteralCheck,
        &AtomicBool::new(false),
    );
    assert_eq!(reason(&a), Some(Reason::CandidateLint));
}
