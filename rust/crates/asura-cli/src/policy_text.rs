//! The fixed policy texts that the CLI and the TUI show
//! (security-policy.md, PL3 control and presentation, and `/status` in
//! PL1). Presentation only: the service owns admission, its results and
//! every decision. Nothing here performs I/O except [`read_text`], which
//! the CLI and the TUI's policy worker call off the render path.
use asura_control::pb;
use asura_control::policy::{MAX_POLICY_TEXT_BYTES, error, state};

/// The `/status` policy line. It reads the last observation only; nothing
/// is shown before one arrives.
pub(crate) fn policy_line(status: Option<&pb::PolicyStatus>) -> Option<String> {
    let status = status?;
    let scope = |state: Option<u32>| match state {
        Some(1) => "ok",
        // PL3: the user scope waits for its re-proof after an update.
        Some(3) => "reproving",
        _ => "unavailable",
    };
    Some(match status.state {
        Some(1) => "policy: starting".into(),
        Some(2) => {
            let revision = match status.revision {
                Some(n) if n > 0 => n.to_string(),
                _ => "pending".into(),
            };
            let changed = match status.repository_changed.unwrap_or(0) {
                0 => String::new(),
                n => format!(" ({n} changed, not admitted)"),
            };
            let solver = match status.solver {
                Some(2) => "available",
                Some(3) => "unavailable",
                _ => "none",
            };
            let approvals = status.approvals_waiting.unwrap_or(0);
            format!(
                "policy: revision {revision} · installation {} · user {} · workspace {} · repository {}{changed} · solver: {solver} · {approvals} approval{} waiting",
                scope(status.installation),
                scope(status.user),
                status.workspace_sets.unwrap_or(0),
                status.repository_sets.unwrap_or(0),
                if approvals == 1 { "" } else { "s" },
            )
        }
        _ => format!(
            "policy: unavailable · installation {} · user {}",
            scope(status.installation),
            scope(status.user)
        ),
    })
}

/// The first 8 hex digits of a candidate ID.
pub(crate) fn short_id(id: &[u8]) -> String {
    id.iter().take(4).map(|b| format!("{b:02x}")).collect()
}
/// All 32 hex digits of a candidate ID.
pub(crate) fn full_id(id: &[u8]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}
/// A 32-hex candidate ID that is not all zero.
pub(crate) fn parse_id(value: &str) -> Option<[u8; 16]> {
    if value.len() != 32 || !value.is_ascii() {
        return None;
    }
    let mut id = [0; 16];
    for (i, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).ok()?;
    }
    (id != [0; 16]).then_some(id)
}

/// A service line shown as printable text: control characters become `?`.
fn printable(line: &str) -> String {
    line.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// The fixed text of a submission's reply.
pub(crate) fn queued_line(reply: &pb::PolicyReply) -> String {
    format!(
        "policy: candidate {} queued (position {})",
        short_id(reply.candidate_id.as_deref().unwrap_or_default()),
        reply.position.unwrap_or(0)
    )
}

/// The fixed text of a `PolicyReply.error` token.
pub(crate) fn error_line(token: &str) -> String {
    let text = match token {
        error::BUSY => "16 candidates are waiting; try again after one finishes",
        error::SCOPE_INVALID => "only the user scope can be admitted",
        error::TEXT_INVALID => "the text is empty or too long",
        error::UNKNOWN_REVISION => "no such revision among the last 64",
        error::NOT_FOUND => "no such candidate is waiting",
        error::UNAVAILABLE => "the policy owner is unavailable",
        _ => "the request failed",
    };
    format!("policy: {text}")
}

/// The candidate's lines: the state line, then for a refusal its detail
/// lines and counterexamples, and for a widening activation its examples.
pub(crate) fn candidate_lines(candidate: &pb::PolicyCandidate) -> Vec<String> {
    let id = short_id(candidate.candidate_id.as_deref().unwrap_or_default());
    let mut lines = Vec::new();
    match candidate.state.unwrap_or(0) {
        state::QUEUED => lines.push(format!(
            "policy: candidate {id} queued (position {})",
            candidate.position.unwrap_or(0)
        )),
        state::PROVING => lines.push(format!("policy: proving candidate {id}")),
        state::ACTIVATED => {
            let examples = candidate.widening_examples.len();
            let widens = if candidate.widening == Some(true) {
                format!(
                    " (widens: {examples} example{})",
                    if examples == 1 { "" } else { "s" }
                )
            } else {
                String::new()
            };
            lines.push(format!(
                "policy: revision {} active: user scope changed{widens}",
                candidate.revision.unwrap_or(0)
            ));
            lines.extend(
                candidate
                    .widening_examples
                    .iter()
                    .map(|e| format!("  widens: {}", printable(e))),
            );
        }
        state::REJECTED => {
            lines.push(format!(
                "policy: candidate {id} refused: {}",
                candidate.reason.as_deref().unwrap_or("refused")
            ));
            lines.extend(
                candidate
                    .details
                    .iter()
                    .map(|d| format!("  {}", printable(d))),
            );
            lines.extend(
                candidate
                    .counterexamples
                    .iter()
                    .map(|c| format!("  counterexample: {}", printable(c))),
            );
        }
        state::CANCELLED => lines.push(format!("policy: candidate {id} cancelled")),
        _ => lines.push(format!("policy: candidate {id}")),
    }
    lines
}

/// The candidate has a result: activated, refused or cancelled.
pub(crate) fn finished(candidate: &pb::PolicyCandidate) -> bool {
    candidate
        .state
        .is_some_and(|s| matches!(s, state::ACTIVATED | state::REJECTED | state::CANCELLED))
}

/// Reads a policy file for submission: at most 61,440 bytes of UTF-8. The
/// person names the file; Asura reads only it, once.
pub(crate) fn read_text(path: &std::path::Path) -> Result<String, String> {
    use std::io::Read;
    let file =
        std::fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_POLICY_TEXT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if bytes.len() > MAX_POLICY_TEXT_BYTES {
        return Err(format!(
            "the policy file is over {MAX_POLICY_TEXT_BYTES} bytes"
        ));
    }
    if bytes.is_empty() {
        return Err("the policy file is empty".into());
    }
    String::from_utf8(bytes).map_err(|_| "the policy file is not UTF-8".into())
}

/// The candidate to show for `id`: the status's candidate when it is
/// `id`, else `id` among the recent finished ones, else the status's
/// candidate (security-policy.md, PL3 control and presentation).
pub(crate) fn shown_candidate(
    status: asura_control::pb::PolicyStatus,
    id: &[u8],
) -> Option<asura_control::pb::PolicyCandidate> {
    let matches = |c: &asura_control::pb::PolicyCandidate| c.candidate_id.as_deref() == Some(id);
    if status.candidate.as_ref().is_some_and(matches) {
        return status.candidate;
    }
    if let Some(found) = status.recent.iter().find(|c| matches(c)) {
        return Some(found.clone());
    }
    status.candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(state: u32) -> pb::PolicyCandidate {
        pb::PolicyCandidate {
            candidate_id: Some(vec![
                0x3f, 0x9c, 0x0a, 0x12, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
            ]),
            state: Some(state),
            scope: Some(2),
            position: Some(1),
            ..Default::default()
        }
    }

    /// The fixed texts of the PL3 contract.
    #[test]
    fn fixed_candidate_texts() {
        assert_eq!(
            candidate_lines(&candidate(state::QUEUED)),
            ["policy: candidate 3f9c0a12 queued (position 1)"]
        );
        assert_eq!(
            candidate_lines(&candidate(state::PROVING)),
            ["policy: proving candidate 3f9c0a12"]
        );
        let active = pb::PolicyCandidate {
            revision: Some(4),
            widening: Some(true),
            widening_examples: vec!["a".into(), "b".into()],
            ..candidate(state::ACTIVATED)
        };
        assert_eq!(
            candidate_lines(&active),
            [
                "policy: revision 4 active: user scope changed (widens: 2 examples)",
                "  widens: a",
                "  widens: b"
            ]
        );
        let narrow = pb::PolicyCandidate {
            revision: Some(5),
            widening: Some(false),
            ..candidate(state::ACTIVATED)
        };
        assert_eq!(
            candidate_lines(&narrow),
            ["policy: revision 5 active: user scope changed"]
        );
        let refused = pb::PolicyCandidate {
            reason: Some("proof_failed".into()),
            details: vec!["user:no-net: person network forbidden (AD-7)".into()],
            counterexamples: vec!["network.connect principal=x\u{1b}".into()],
            ..candidate(state::REJECTED)
        };
        assert_eq!(
            candidate_lines(&refused),
            [
                "policy: candidate 3f9c0a12 refused: proof_failed",
                "  user:no-net: person network forbidden (AD-7)",
                "  counterexample: network.connect principal=x?"
            ]
        );
        assert_eq!(
            candidate_lines(&candidate(state::CANCELLED)),
            ["policy: candidate 3f9c0a12 cancelled"]
        );
        assert!(finished(&candidate(state::CANCELLED)));
        assert!(!finished(&candidate(state::PROVING)));
        let reply = pb::PolicyReply {
            candidate_id: Some(vec![
                0x3f, 0x9c, 0x0a, 0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
            ]),
            position: Some(2),
            error: None,
        };
        assert_eq!(
            queued_line(&reply),
            "policy: candidate 3f9c0a12 queued (position 2)"
        );
        for token in error::ALL {
            assert_ne!(error_line(token), "policy: the request failed");
        }
    }

    #[test]
    fn ids_and_files() {
        let id = [7u8; 16];
        assert_eq!(parse_id(&full_id(&id)), Some(id));
        assert_eq!(parse_id(&"0".repeat(32)), None);
        assert_eq!(parse_id("07"), None);
        let dir = std::env::temp_dir().join(format!("asura-policy-text-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ok = dir.join("ok.cedar");
        std::fs::write(&ok, "forbid (principal, action, resource);").unwrap();
        assert!(read_text(&ok).is_ok());
        let big = dir.join("big.cedar");
        std::fs::write(&big, vec![b'a'; MAX_POLICY_TEXT_BYTES + 1]).unwrap();
        assert!(read_text(&big).is_err());
        let max = dir.join("max.cedar");
        std::fs::write(&max, vec![b'a'; MAX_POLICY_TEXT_BYTES]).unwrap();
        assert!(read_text(&max).is_ok());
        let empty = dir.join("empty.cedar");
        std::fs::write(&empty, b"").unwrap();
        assert!(read_text(&empty).is_err());
        let binary = dir.join("binary.cedar");
        std::fs::write(&binary, [0xff, 0xfe]).unwrap();
        assert!(read_text(&binary).is_err());
        assert!(read_text(&dir.join("missing")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
