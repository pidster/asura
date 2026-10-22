//! `/execution` (execution-setting.md, TUI and ES-F2): reads and sets the
//! EX11 keys through the existing configuration worker. The service owns the
//! effective value, refusal and revocation; the TUI only shows them. The
//! `exec off` status item comes from the job worker's status `JobList`.
use super::*;

const USAGE: &str =
    "Use /execution, /execution on|off or /execution project on|off|inherit; draft retained";

/// What a pending `/execution` request reads or sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Execution {
    Read {
        project: Option<[u8; 16]>,
    },
    Global(bool),
    /// `Some(true)` on, `Some(false)` off, `None` inherit.
    Project {
        project: [u8; 16],
        name: String,
        value: Option<bool>,
    },
}

/// The configuration key text of a project override (execution-setting.md,
/// configuration): `execution.projects.<32 lowercase hex digits>`.
pub(super) fn project_key(project: &[u8; 16]) -> String {
    format!("execution.projects.{}", crate::conversation::hex(project))
}

/// The global default and one project's override from the `execution`
/// mapping as the configuration worker returns it: `enabled: BOOL` and
/// `projects:` with `ID: on|off` entries (the ID may be quoted).
pub(super) fn parse_mapping(
    yaml: &str,
    project: Option<&[u8; 16]>,
) -> Option<(bool, Option<bool>)> {
    let mut enabled = None;
    let mut over = None;
    let id = project.map(|p| crate::conversation::hex(p));
    for line in yaml.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("enabled:") {
            enabled = match value.trim() {
                "true" => Some(true),
                "false" => Some(false),
                _ => return None,
            };
        } else if let Some(id) = &id
            && let Some((key, value)) = trimmed.split_once(':')
            && key.trim().trim_matches(|c| c == '\'' || c == '"') == id
        {
            over = match value.trim() {
                "on" => Some(true),
                "off" => Some(false),
                _ => return None,
            };
        }
    }
    Some((enabled?, over))
}

fn on(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

/// `execution: on (default) · this project: off (project override)`.
pub(super) fn summary(enabled: bool, project: Option<Option<bool>>) -> String {
    let mut text = format!("execution: {} (default)", on(enabled));
    match project {
        Some(Some(value)) => text.push_str(&format!(
            " · this project: {} (project override)",
            on(value)
        )),
        Some(None) => text.push_str(&format!(" · this project: {} (inherited)", on(enabled))),
        None => {}
    }
    text
}

impl App {
    pub(super) fn execution_command(&mut self, draft: String) {
        let words: Vec<_> = draft.split_whitespace().collect();
        let command = match words.as_slice() {
            ["/execution"] => Execution::Read {
                project: self.selected_project.filter(|p| self.registered(*p)),
            },
            ["/execution", value @ ("on" | "off")] => Execution::Global(*value == "on"),
            ["/execution", "project", value @ ("on" | "off" | "inherit")] => {
                let Some(project) = self.selected_project.filter(|p| self.registered(*p)) else {
                    self.notice = "Select a project first; draft retained".into();
                    return;
                };
                Execution::Project {
                    project,
                    name: self.project_name(project),
                    value: match *value {
                        "on" => Some(true),
                        "off" => Some(false),
                        _ => None,
                    },
                }
            }
            _ => {
                self.notice = USAGE.into();
                return;
            }
        };
        if self.config_draft.is_some() || self.picker_model_save.is_some() {
            self.notice = "Configuration request pending; draft retained".into();
            return;
        }
        let (key, value) = match &command {
            Execution::Read { .. } => ("execution".to_owned(), None),
            Execution::Global(value) => ("execution.enabled".to_owned(), Some(value.to_string())),
            Execution::Project { project, value, .. } => (
                project_key(project),
                Some(
                    match value {
                        Some(true) => "on",
                        Some(false) => "off",
                        None => "inherit",
                    }
                    .to_owned(),
                ),
            ),
        };
        self.config_request = Some(super::super::config::Request {
            draft: draft.clone(),
            key,
            value,
        });
        self.config_draft = Some(draft);
        self.execution = Some(command);
        self.notice = "Execution setting request pending · Esc stops waiting".into();
    }
    fn registered(&self, project: [u8; 16]) -> bool {
        self.project_row(project)
            .is_some_and(|row| row.current == Some(true))
    }
    /// Applies a configuration result of `/execution`; false for others.
    pub(super) fn execution_finished(&mut self, outcome: &super::super::config::Outcome) -> bool {
        let Some(command) = self.execution.take() else {
            return false;
        };
        let submitted = self.config_draft.take();
        let value = match outcome {
            Err(error) if error.contains("unconfirmed") || error.contains("stopped") => {
                self.notice =
                    "Execution setting outcome unconfirmed; read it again with /execution".into();
                return true;
            }
            Err(error) => {
                self.notice = format!(
                    "Execution setting not changed: {}; draft retained",
                    literal(error, 256)
                );
                return true;
            }
            // The worker returns the key line, then the YAML value.
            Ok(text) => text.split_once('\n').map_or("", |(_, value)| value),
        };
        if submitted.as_deref() == Some(self.editor.text().as_str()) {
            self.editor = Editor::new();
        }
        self.notice = match command {
            Execution::Read { project } => match parse_mapping(value, project.as_ref()) {
                Some((enabled, over)) => summary(enabled, project.map(|_| over)),
                None => "Execution setting reply unreadable".into(),
            },
            Execution::Global(true) => "execution: on (default) · projects without an override run model commands".into(),
            Execution::Global(false) => "execution: off (default) · model jobs in projects without an override are being stopped · /jobs".into(),
            Execution::Project {
                name,
                value: Some(false),
                ..
            } => format!(
                "execution off for {name}: running model jobs in this project are being stopped · /jobs"
            ),
            Execution::Project {
                name,
                value: Some(true),
                ..
            } => format!("execution on for {name} (project override)"),
            Execution::Project {
                name, value: None, ..
            } => format!("execution for {name} follows the default"),
        };
        self.notice = literal(&self.notice, 1024);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn registered_app() -> App {
        let mut app = App::new();
        app.projects = Some(vec![asura_control::pb::ProjectReply {
            project_id: Some(vec![0xab; 16]),
            name: Some("demo".into()),
            current: Some(true),
            ..Default::default()
        }]);
        app.selected_project = Some([0xab; 16]);
        app
    }
    fn run(app: &mut App, draft: &str) {
        app.editor = Editor::new();
        app.insert(draft);
        app.submit();
    }

    #[test]
    fn es_03_mapping_and_summary_show_each_source() {
        let id = [0xab; 16];
        let hex = crate::conversation::hex(&id);
        assert_eq!(
            parse_mapping("enabled: true\nprojects: {}\n", Some(&id)),
            Some((true, None))
        );
        assert_eq!(
            parse_mapping(
                &format!("enabled: false\nprojects:\n  {hex}: on\n"),
                Some(&id)
            ),
            Some((false, Some(true)))
        );
        assert_eq!(
            parse_mapping(
                &format!("enabled: true\nprojects:\n  '{hex}': off\n"),
                Some(&id)
            ),
            Some((true, Some(false)))
        );
        assert_eq!(parse_mapping("projects: {}\n", None), None);
        // All four combinations of global and override.
        assert_eq!(
            summary(true, Some(Some(false))),
            "execution: on (default) · this project: off (project override)"
        );
        assert_eq!(
            summary(false, Some(Some(true))),
            "execution: off (default) · this project: on (project override)"
        );
        assert_eq!(
            summary(true, Some(None)),
            "execution: on (default) · this project: on (inherited)"
        );
        assert_eq!(
            summary(false, Some(None)),
            "execution: off (default) · this project: off (inherited)"
        );
        assert_eq!(summary(true, None), "execution: on (default)");
        assert_eq!(
            project_key(&id),
            format!("execution.projects.{}", "ab".repeat(16))
        );
    }

    #[test]
    fn es_f2_execution_command_parses_dispatches_and_reports() {
        // Usage keeps the draft and sends nothing.
        let mut app = registered_app();
        for bad in [
            "/execution maybe",
            "/execution project",
            "/execution on off",
        ] {
            run(&mut app, bad);
            assert!(app.config_request.is_none(), "{bad}");
            assert_eq!(app.draft(), bad);
            assert!(app.notice.starts_with("Use /execution"), "{bad}");
        }
        // Project forms need a selected registered project.
        let mut none = App::new();
        run(&mut none, "/execution project off");
        assert!(none.config_request.is_none());
        assert_eq!(none.notice, "Select a project first; draft retained");

        // Read: ConfigGet execution; the result shows both sources.
        run(&mut app, "/execution");
        let request = app.take_config_request().unwrap();
        assert_eq!((request.key.as_str(), request.value), ("execution", None));
        // A second configuration request while one is pending is refused.
        app.editor = Editor::new();
        app.insert("/execution off");
        app.submit();
        assert_eq!(app.notice, "Configuration request pending; draft retained");
        app.editor = Editor::new();
        app.insert("/execution");
        let hex = "ab".repeat(16);
        app.config_finished(Ok(format!(
            "execution\nenabled: true\nprojects:\n  {hex}: off\n"
        )));
        assert_eq!(
            app.notice,
            "execution: on (default) · this project: off (project override)"
        );
        assert_eq!(app.draft(), "", "the submitted draft clears on success");
        assert!(app.config_result.is_none(), "no configuration panel");

        // Set the selected project's override: the leaf key.
        run(&mut app, "/execution project off");
        let request = app.take_config_request().unwrap();
        assert_eq!(request.key, format!("execution.projects.{hex}"));
        assert_eq!(request.value.as_deref(), Some("off"));
        app.config_finished(Ok(format!("Saved execution.projects.{hex}\noff\n")));
        assert_eq!(
            app.notice,
            "execution off for demo: running model jobs in this project are being stopped · /jobs"
        );
        run(&mut app, "/execution project inherit");
        assert_eq!(
            app.take_config_request().unwrap().value.as_deref(),
            Some("inherit")
        );
        app.config_finished(Ok("Saved\ninherit\n".into()));
        assert_eq!(app.notice, "execution for demo follows the default");

        // Global: execution.enabled.
        run(&mut app, "/execution off");
        let request = app.take_config_request().unwrap();
        assert_eq!(
            (request.key.as_str(), request.value.as_deref()),
            ("execution.enabled", Some("false"))
        );
        // Validation error: nothing changed; the draft stays.
        app.config_finished(Err("config_invalid_value".into()));
        assert!(app.notice.contains("not changed: config_invalid_value"));
        assert_eq!(app.draft(), "/execution off");
        // Cancel: outcome unconfirmed.
        run(&mut app, "/execution on");
        assert!(app.take_config_request().is_some());
        assert!(app.cancel_current(false));
        assert!(app.take_config_cancel());
        app.config_finished(Err(
            "Configuration wait stopped; outcome unconfirmed, use /config get to check".into(),
        ));
        assert_eq!(
            app.notice,
            "Execution setting outcome unconfirmed; read it again with /execution"
        );
        // A later /config result is not taken for /execution.
        run(&mut app, "/config get model");
        app.take_config_request().unwrap();
        app.config_finished(Ok("model\nollama:x\n".into()));
        assert!(app.config_result.is_some());
    }
}
