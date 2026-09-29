//! Presentation commands; authority remains in the service.
use asura_control::pb;
#[derive(Clone, Debug)]
pub(crate) enum Command {
    Initialize([u8; 16]),
    Register([u8; 16], String),
    List,
    Observe([u8; 16]),
}
pub(crate) fn hex(id: &[u8]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}
pub(crate) fn project_display_name(project: &pb::ProjectReply) -> String {
    if let Some(name) = project.name.as_deref().filter(|name| !name.is_empty()) {
        return name.into();
    }
    let path = project.location.as_deref().unwrap_or_default();
    let component = std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let fallback = || {
        let id = project.project_id.as_deref().unwrap_or_default();
        format!("Project {}", hex(&id[..id.len().min(4)]))
    };
    if component.is_empty()
        || component.len() > 128
        || component.starts_with(' ')
        || component.ends_with(' ')
        || !component.chars().any(char::is_alphabetic)
        || component.chars().any(|ch| {
            ch.is_control()
                || matches!(ch, '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
    {
        return fallback();
    }
    let mut result = String::with_capacity(component.len());
    let mut capitalized = false;
    for ch in component.chars() {
        if !capitalized && ch.is_alphabetic() {
            result.extend(ch.to_uppercase());
            capitalized = true;
        } else {
            result.push(ch);
        }
    }
    if result.len() > 128 {
        fallback()
    } else {
        result
    }
}
pub(crate) fn project_list_line(project: &pb::ProjectReply) -> String {
    format!(
        "{}  {}  {}{}",
        hex(project.project_id.as_deref().unwrap_or_default()),
        project_display_name(project),
        project.location.as_deref().unwrap_or_default(),
        if project.current == Some(true) {
            ""
        } else {
            " [stale]"
        }
    )
}
pub(crate) fn parse_id(text: &str) -> Result<[u8; 16], String> {
    if text.len() != 32 || !text.is_ascii() {
        return Err("Expected a 32-digit project or operation ID".into());
    }
    let mut id = [0; 16];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).map_err(|_| "Invalid ID")?;
    }
    if id == [0; 16] {
        return Err("Zero ID is invalid".into());
    }
    Ok(id)
}
pub(crate) fn location(path: &str) -> Result<String, String> {
    let path = std::path::Path::new(path);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    let mut normalized = std::path::PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            part => normalized.push(part),
        }
    }
    normalized
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| "Project path must be UTF-8".into())
}
pub(crate) fn execute(
    client: &mut asura_client::Client,
    command: Command,
) -> Result<(String, Option<[u8; 16]>), String> {
    match command {
        Command::Initialize(id) => {
            let reply = match client.initialize(pb::InitializeInstallation {
                request_id: Some(id.to_vec()),
                mode: Some("embedded".into()),
                expected_authority_revision: Some(0),
            }) {
                Ok(reply) => reply,
                Err(asura_client::Error::Remote(_, reason)) => {
                    return initialization_rejected(&reason, &id);
                }
                Err(error) => {
                    return Err(format!(
                        "{error}; outcome unconfirmed; request {}",
                        hex(&id)
                    ));
                }
            };
            if let Some(error) = reply.error {
                return initialization_rejected(&error, &id);
            }
            Ok((
                format!(
                    "Installation {} · {}",
                    hex(reply.installation_id.as_deref().unwrap_or_default()),
                    if reply.phase == Some(2) {
                        "ready"
                    } else {
                        "pending"
                    }
                ),
                None,
            ))
        }
        Command::Register(id, path) => {
            let reply = client
                .register_project(pb::ProjectRegister {
                    request_id: Some(id.to_vec()),
                    location: Some(path),
                })
                .map_err(|e| format!("{e}; outcome unconfirmed; request {}", hex(&id)))?;
            if let Some(error) = reply.error {
                return Err(error);
            }
            let id: [u8; 16] = reply
                .project_id
                .ok_or("Missing project ID")?
                .try_into()
                .map_err(|_| "Invalid project ID")?;
            Ok((
                format!(
                    "Project {} · {}",
                    hex(&id),
                    reply.location.unwrap_or_default()
                ),
                Some(id),
            ))
        }
        Command::List => {
            let lines = project_registry(client, || Ok(()))?
                .into_iter()
                .map(|project| project_list_line(&project))
                .collect::<Vec<_>>();
            Ok((
                if lines.is_empty() {
                    "No projects. Use /project add PATH".into()
                } else {
                    lines.join("\n")
                },
                None,
            ))
        }
        Command::Observe(_) => Err("Observation requires the conversation worker".into()),
    }
}

/// Complete bounded registry snapshot; never publish a partial page sequence.
pub(crate) fn project_registry(
    client: &mut asura_client::Client,
    check: impl FnMut() -> Result<(), String>,
) -> Result<Vec<pb::ProjectReply>, String> {
    collect_projects(
        |after| client.projects(after).map_err(|e| e.to_string()),
        check,
    )
}

fn collect_projects(
    mut fetch: impl FnMut(Option<[u8; 16]>) -> Result<pb::ProjectsReply, String>,
    mut check: impl FnMut() -> Result<(), String>,
) -> Result<Vec<pb::ProjectReply>, String> {
    let mut after = None;
    let mut projects = Vec::new();
    let mut ids = std::collections::BTreeSet::new();
    for _ in 0..8 {
        check()?;
        let reply = fetch(after)?;
        check()?;
        if let Some(error) = reply.error {
            return Err(error);
        }
        if reply.projects.len() > 8 {
            return Err("Invalid project page size".into());
        }
        let mut last = after;
        for project in &reply.projects {
            let id = registry_id(project.project_id.as_deref())?;
            if project.error.is_some()
                || project.current.is_none()
                || project.registry_revision != Some(1)
                || project.location.as_deref().is_none_or(|path| {
                    !path.starts_with('/')
                        || path.len() > 4096
                        || path.contains('\0')
                        || path.split('/').any(|p| matches!(p, "." | ".."))
                })
                || project.name.as_deref().is_some_and(|name| {
                    name.is_empty()
                        || name.len() > 128
                        || name.starts_with(' ')
                        || name.ends_with(' ')
                        || name.chars().any(|ch| {
                            ch.is_control()
                                || matches!(ch, '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
                        })
                })
                || last.is_some_and(|previous| id <= previous)
                || !ids.insert(id)
            {
                return Err("Invalid project registry response".into());
            }
            last = Some(id);
        }
        let next = reply
            .next_cursor
            .as_deref()
            .map(|v| registry_id(Some(v)))
            .transpose()?;
        if next.is_some() && (reply.projects.is_empty() || next != last || next <= after) {
            return Err("Invalid project page cursor".into());
        }
        projects.extend(reply.projects);
        if next.is_none() {
            return Ok(projects);
        }
        after = next;
    }
    Err("Project registry exceeds pagination limit".into())
}

fn registry_id(value: Option<&[u8]>) -> Result<[u8; 16], String> {
    let id: [u8; 16] = value
        .ok_or("Missing project ID")?
        .try_into()
        .map_err(|_| "Invalid project ID")?;
    if id == [0; 16] {
        return Err("Zero project ID".into());
    }
    Ok(id)
}

pub(crate) fn initial_project(projects: &[pb::ProjectReply], launch: &str) -> Option<[u8; 16]> {
    let current = projects
        .iter()
        .filter(|p| p.current == Some(true))
        .collect::<Vec<_>>();
    let matched = current
        .iter()
        .filter(|p| {
            p.location
                .as_deref()
                .is_some_and(|path| std::path::Path::new(launch).starts_with(path))
        })
        .max_by_key(|p| {
            std::path::Path::new(p.location.as_deref().unwrap_or_default())
                .components()
                .count()
        });
    let selected = matched
        .copied()
        .or_else(|| (current.len() == 1).then(|| current[0]));
    selected.and_then(|p| registry_id(p.project_id.as_deref()).ok())
}

fn initialization_rejected(
    reason: &str,
    id: &[u8; 16],
) -> Result<(String, Option<[u8; 16]>), String> {
    match reason {
        "installation_already_initialized" => Ok((
            "Installation already initialized.\nUse /project list or /project add PATH.".into(),
            None,
        )),
        "conversation_busy" => {
            Err("Service is starting or busy. Wait, then use /retry. Draft retained.".into())
        }
        "outcome_unconfirmed" => Err(format!(
            "Outcome unconfirmed; request {}; use /retry",
            hex(id)
        )),
        _ => Err(reason.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(n: u8, location: &str, current: bool) -> pb::ProjectReply {
        pb::ProjectReply {
            project_id: Some(vec![n; 16]),
            location: Some(location.into()),
            registry_revision: Some(1),
            current: Some(current),
            error: None,
            ..Default::default()
        }
    }
    #[test]
    fn project_display_name_capitalizes_first_letter_and_keeps_location_context() {
        let mut project = record(1, "/work/my-project", true);
        assert_eq!(project_display_name(&project), "My-project");
        project.name = Some("Shared name".into());
        assert_eq!(project_display_name(&project), "Shared name");
        let line = project_list_line(&project);
        assert!(line.contains("Shared name  /work/my-project"), "{line}");
        project.name = None;
        project.location = Some("/".into());
        assert_eq!(project_display_name(&project), "Project 01010101");
    }
    #[test]
    fn registry_collects_pages_and_checks_cancellation() {
        let mut calls = 0;
        let projects = collect_projects(
            |after| {
                calls += 1;
                assert_eq!(after, if calls == 1 { None } else { Some([1; 16]) });
                Ok(pb::ProjectsReply {
                    projects: vec![record(calls, "/project", true)],
                    next_cursor: (calls == 1).then(|| vec![1; 16]),
                    error: None,
                })
            },
            || Ok(()),
        )
        .unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(
            collect_projects(|_| panic!("cancelled fetch"), || Err("cancelled".into()))
                .unwrap_err(),
            "cancelled"
        );
    }
    #[test]
    fn registry_rejects_invalid_and_incomplete_pages() {
        for project in [record(0, "/a", true), record(1, "relative", true)] {
            assert!(
                collect_projects(
                    |_| Ok(pb::ProjectsReply {
                        projects: vec![project.clone()],
                        next_cursor: None,
                        error: None
                    }),
                    || Ok(())
                )
                .is_err()
            );
        }
        assert!(
            collect_projects(
                |_| Ok(pb::ProjectsReply {
                    projects: vec![record(1, "/a", true)],
                    next_cursor: Some(vec![1; 16]),
                    error: None
                }),
                || Ok(())
            )
            .is_err()
        );
        assert!(
            collect_projects(
                |_| Ok(pb::ProjectsReply {
                    projects: vec![],
                    next_cursor: Some(vec![1; 16]),
                    error: None
                }),
                || Ok(())
            )
            .is_err()
        );
        let mut calls = 0;
        assert!(
            collect_projects(
                |_| {
                    calls += 1;
                    Ok(pb::ProjectsReply {
                        projects: vec![record(calls, "/a", true)],
                        next_cursor: Some(vec![calls; 16]),
                        error: None,
                    })
                },
                || Ok(())
            )
            .is_err()
        );
        assert_eq!(calls, 8);
        assert_eq!(
            collect_projects(
                |_| Ok(pb::ProjectsReply {
                    projects: vec![],
                    next_cursor: None,
                    error: Some("unavailable".into())
                }),
                || Ok(())
            )
            .unwrap_err(),
            "unavailable"
        );
    }
    #[test]
    fn selection_uses_deepest_current_component_match_or_sole_current() {
        let projects = vec![
            record(1, "/work", true),
            record(2, "/work/sub", true),
            record(3, "/work/sub/deep", false),
        ];
        assert_eq!(
            initial_project(&projects, "/work/sub/deep/file"),
            Some([2; 16])
        );
        assert_eq!(initial_project(&projects, "/workspace"), None);
        assert_eq!(
            initial_project(
                &[record(1, "/other", true), record(2, "/stale", false)],
                "/here"
            ),
            Some([1; 16])
        );
        assert_eq!(initial_project(&[record(1, "/here", false)], "/here"), None);
        assert_eq!(initial_project(&[], "/here"), None);
    }
    #[test]
    fn initialization_rejections_distinguish_existing_busy_conflict_and_uncertainty() {
        let (message, project) =
            initialization_rejected("installation_already_initialized", &[1; 16]).unwrap();
        assert!(message.contains("Installation already initialized."));
        assert!(project.is_none());
        let busy = initialization_rejected("conversation_busy", &[1; 16]).unwrap_err();
        assert!(busy.contains("/retry"));
        assert!(!busy.contains("unconfirmed"));
        assert_eq!(
            initialization_rejected("request_conflict", &[1; 16]).unwrap_err(),
            "request_conflict"
        );
        let uncertain = initialization_rejected("outcome_unconfirmed", &[1; 16]).unwrap_err();
        assert!(uncertain.contains(&hex(&[1; 16])));
        assert!(uncertain.contains("/retry"));
    }
}
