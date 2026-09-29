//! Standalone sensor inspection; presentation only, no sensor policy.
use super::{BUILD, Client, Duration, Instant, RuntimeResolver};
use asura_control::pb;
pub(super) fn parse_id(value: &str) -> Result<[u8; 16], ()> {
    if value.len() != 32 || !value.is_ascii() {
        return Err(());
    }
    let mut id = [0; 16];
    for (i, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).map_err(|_| ())?;
    }
    if id == [0; 16] {
        return Err(());
    }
    Ok(id)
}
fn report(page: &pb::SensorsReply) -> String {
    use std::fmt::Write;
    let project = page
        .project_id
        .as_ref()
        .map(|id| id.iter().map(|b| format!("{b:02x}")).collect::<String>())
        .unwrap_or_default();
    let mut output = format!(
        "Sensors {project} · revision {}\nPending persistence: {} · intake unavailable: {} · clock uncertain: {}\n",
        page.revision.unwrap_or(0),
        page.pending_persistence.unwrap_or(false),
        page.intake_unavailable.unwrap_or(false),
        page.clock_uncertain.unwrap_or(false)
    );
    output.push_str("Source          Sequence  Expires (Unix ms)\n");
    for row in &page.observations {
        let source = match row.source {
            Some(1) => "activity",
            Some(2) => "idle",
            Some(3) => "service_status",
            _ => "unknown",
        };
        let _ = writeln!(
            output,
            "{source:<15} {:>8}  {}",
            row.sequence.unwrap_or(0),
            row.expires_ms.unwrap_or(0)
        );
    }
    if page.observations.is_empty() {
        output.push_str("No committed observations.\n");
    }
    output.push_str("Purpose        State        Reason\n");
    for row in &page.proposals {
        let purpose = match row.purpose {
            Some(1) => "consolidation",
            Some(2) => "reflection",
            _ => "unknown",
        };
        let state = match row.state {
            Some(1) => "held",
            Some(2) => "expired",
            Some(3) => "invalidated",
            _ => "unknown",
        };
        let reason = match row.reason {
            Some(1) => "sensor_admission_unavailable",
            Some(2) => "clock_uncertain",
            Some(3) => "activity_resumed",
            Some(4) => "expired",
            _ => "unknown",
        };
        let _ = writeln!(output, "{purpose:<14} {state:<12} {reason}");
    }
    if page.proposals.is_empty() {
        output.push_str("No committed proposals.\n");
    }
    if let Some(offset) = page.next_offset {
        let _ = writeln!(
            output,
            "Next: asura sensors {project} {offset} {}",
            page.revision.unwrap_or(0)
        );
    }
    output
}
pub(super) fn run(
    resolve: RuntimeResolver,
    project: [u8; 16],
    offset: u32,
    revision: Option<u64>,
) -> i32 {
    let result = (|| {
        let runtime = resolve(false).map_err(|e| e.to_string())?;
        let mut client = Client::attach(&runtime, BUILD, Instant::now() + Duration::from_secs(2))
            .map_err(|e| e.to_string())?;
        let reply = client
            .sensors(pb::SensorsInspect {
                project_id: Some(project.to_vec()),
                offset: Some(offset),
                revision,
                limit: Some(16),
            })
            .map_err(|e| e.to_string())?;
        if let Some(reason) = reply.error {
            return Err(reason);
        }
        Ok(report(&reply))
    })();
    match result {
        Ok(output) => {
            print!("{output}");
            0
        }
        Err(reason) => {
            tracing::error!("{reason}");
            3
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sensor_id_is_strict_and_nonzero() {
        assert_eq!(parse_id(&"12".repeat(16)), Ok([0x12; 16]));
        for value in [
            "00".repeat(16),
            "xx".repeat(16),
            "é".repeat(16),
            "1".repeat(31),
        ] {
            assert!(parse_id(&value).is_err());
        }
    }
    #[test]
    fn cursor_arguments_require_offset_and_revision_together() {
        let id = "12".repeat(16);
        for parts in [
            vec!["sensors", &id, "1"],
            vec!["sensors", &id, "65", "2"],
            vec!["sensors", &id, "-1", "2"],
            vec!["sensors", &id, "1", "bad"],
        ] {
            let args = parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            assert!(super::super::parse(&args).is_err());
        }
        let args = ["sensors", &id, "16", "3"].map(str::to_string);
        assert!(matches!(
            super::super::parse(&args),
            Ok(super::super::Command::Sensors {
                offset: 16,
                revision: Some(3),
                ..
            })
        ));
    }
    #[test]
    fn empty_and_continuation_reports_expose_health() {
        let page = pb::SensorsReply {
            project_id: Some(vec![1; 16]),
            revision: Some(3),
            pending_persistence: Some(true),
            next_offset: Some(16),
            ..Default::default()
        };
        let output = report(&page);
        assert!(output.contains("Pending persistence: true"));
        assert!(output.contains("No committed observations."));
        assert!(output.contains(&format!("Next: asura sensors {} 16 3", "01".repeat(16))));
    }
}
