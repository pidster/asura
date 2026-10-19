//! Presentation commands; authority remains in the service.
use asura_control::pb;
#[derive(Clone, Debug)]
pub(crate) enum Command {
    Initialize([u8; 16]),
    Register([u8; 16], String),
    /// Fill the project window from the first page, or after a cursor.
    List(Option<[u8; 16]>),
    Observe([u8; 16]),
}
pub(crate) use asura_client::ProjectPage;
/// The client's project window: at most 8 pages (CF06b project paging).
pub(crate) const WINDOW: usize = 64;
/// The entries one project page may hold.
pub(crate) const PAGE: u32 = 8;
/// Shown for `journal_quota`; the person's draft is kept.
pub(crate) const JOURNAL_QUOTA_NOTICE: &str =
    "Journal space is full. Raise journal.quotaBytes or free disk space.";
/// Shown for `journal_disk_space`; the person's draft is kept.
pub(crate) const JOURNAL_DISK_NOTICE: &str = "Disk space is low for the journal.";
/// The person-facing text of a service error code. The two journal hold
/// reasons (segmented journal, holding and resuming) have fixed notices;
/// every other code is shown as it is.
pub(crate) fn service_error(code: &str) -> String {
    journal_notice(code).map_or_else(|| code.into(), str::to_owned)
}
/// A definite rejection that keeps the draft: a journal hold reason shows
/// its notice; any other code is named after `action`.
pub(crate) fn rejection(action: &str, code: &str) -> String {
    match journal_notice(code) {
        Some(notice) => format!("{notice} Draft retained."),
        None => format!("{action}: {code}; draft retained"),
    }
}
pub(crate) fn journal_notice(code: &str) -> Option<&'static str> {
    match code {
        "journal_quota" => Some(JOURNAL_QUOTA_NOTICE),
        "journal_disk_space" => Some(JOURNAL_DISK_NOTICE),
        _ => None,
    }
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
/// The one reason a listed project may carry; see the state isolation design.
pub(crate) const OVERLAPS_ASURA_STATE: &str = "project_overlaps_asura_state";
pub(crate) fn overlaps_asura_state(project: &pb::ProjectReply) -> bool {
    project.error.as_deref() == Some(OVERLAPS_ASURA_STATE)
}
pub(crate) fn project_list_line(project: &pb::ProjectReply) -> String {
    format!(
        "{}  {}  {}{}",
        hex(project.project_id.as_deref().unwrap_or_default()),
        project_display_name(project),
        project.location.as_deref().unwrap_or_default(),
        if project.current == Some(true) {
            ""
        } else if overlaps_asura_state(project) {
            " [unavailable: contains Asura state]"
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
/// `cancelled` ends only the wait of a journal-backed mutation (initialize,
/// register), which has no time bound (J1-0). Reads keep their client budgets.
pub(crate) fn execute(
    client: &mut asura_client::Client,
    command: Command,
    cancelled: &dyn Fn() -> bool,
) -> Result<(String, Option<[u8; 16]>), String> {
    match command {
        Command::Initialize(id) => {
            let reply = match client.initialize(
                pb::InitializeInstallation {
                    request_id: Some(id.to_vec()),
                    mode: Some("embedded".into()),
                    expected_authority_revision: Some(0),
                },
                cancelled,
            ) {
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
                .register_project(
                    pb::ProjectRegister {
                        request_id: Some(id.to_vec()),
                        location: Some(path),
                    },
                    cancelled,
                )
                .map_err(|e| match e {
                    asura_client::Error::Remote(_, reason) if journal_notice(&reason).is_some() => {
                        service_error(&reason)
                    }
                    e => format!("{e}; outcome unconfirmed; request {}", hex(&id)),
                })?;
            if let Some(error) = reply.error {
                return Err(service_error(&error));
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
        Command::List(_) => Err("Project listing requires a page reader".into()),
        Command::Observe(_) => Err("Observation requires the conversation worker".into()),
    }
}

/// One validated project page, in ascending ID order for display. `next` is
/// the cursor of the following page in the read's own order.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Page {
    pub rows: Vec<pb::ProjectReply>,
    pub next: Option<[u8; 16]>,
}

/// The client's project window: consecutive registry entries in ascending ID
/// order, at most [`WINDOW`]. `more_before` and `more_after` say whether a
/// read in that direction may find more entries.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Window {
    pub rows: Vec<pb::ProjectReply>,
    pub more_before: bool,
    pub more_after: bool,
}
impl Window {
    /// The `/project list after ID` hint when more entries follow the window.
    pub(crate) fn more_hint(&self) -> Option<String> {
        if !self.more_after {
            return None;
        }
        let last = self.rows.last()?.project_id.as_deref()?;
        Some(format!("More: /project list after {}", hex(last)))
    }
    /// The window's lines, the empty-registry instruction, and the hint.
    pub(crate) fn message(&self) -> String {
        if self.rows.is_empty() && !self.more_before {
            return "No projects. Use /project add PATH".into();
        }
        let mut lines = self.rows.iter().map(project_list_line).collect::<Vec<_>>();
        if self.rows.is_empty() {
            lines.push("No projects after this ID".into());
        }
        lines.extend(self.more_hint());
        lines.join("\n")
    }
    /// Joins a page read at an edge: `after` pages append and `before` pages
    /// prepend; entries at the other end are dropped so the window stays
    /// within [`WINDOW`]. A page that does not start at this window's edge
    /// is rejected, so joins have no gap or duplicate.
    pub(crate) fn join(&mut self, read: ProjectPage, page: Page) -> Result<(), String> {
        let first = self.rows.first().and_then(row_id);
        let last = self.rows.last().and_then(row_id);
        match read {
            ProjectPage::After(cursor) if Some(cursor) == last => {
                self.more_after = page.next.is_some();
                self.rows.extend(page.rows);
                if self.rows.len() > WINDOW {
                    self.rows.drain(..self.rows.len() - WINDOW);
                    self.more_before = true;
                }
            }
            ProjectPage::Before(cursor) if Some(cursor) == first => {
                self.more_before = page.next.is_some();
                let mut rows = page.rows;
                rows.append(&mut self.rows);
                self.rows = rows;
                if self.rows.len() > WINDOW {
                    self.rows.truncate(WINDOW);
                    self.more_after = true;
                }
            }
            _ => return Err("Project page does not join the list".into()),
        }
        Ok(())
    }
}
fn row_id(row: &pb::ProjectReply) -> Option<[u8; 16]> {
    row.project_id.as_deref()?.try_into().ok()
}

fn valid_row(project: &pb::ProjectReply) -> bool {
    let reason_valid = project.error.is_none()
        || (overlaps_asura_state(project) && project.current == Some(false));
    reason_valid
        && project.current.is_some()
        && project.registry_revision == Some(1)
        && project.location.as_deref().is_some_and(|path| {
            path.starts_with('/')
                && path.len() <= 4096
                && !path.contains('\0')
                && !path.split('/').any(|p| matches!(p, "." | ".."))
        })
        && project.name.as_deref().is_none_or(|name| {
            !name.is_empty()
                && name.len() <= 128
                && !name.starts_with(' ')
                && !name.ends_with(' ')
                && !name.chars().any(|ch| {
                    ch.is_control()
                        || matches!(ch, '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
                })
        })
}

/// Validates one reply against the read that asked for it: page size, entry
/// fields, strict ID order past the cursor (descending for `before`) and the
/// next cursor, which must be the last entry. `only` returns that project or
/// nothing, never a cursor. A `before` page is reversed for display.
pub(crate) fn validate_page(
    read: ProjectPage,
    limit: u32,
    reply: pb::ProjectsReply,
) -> Result<Page, String> {
    if let Some(error) = reply.error {
        return Err(service_error(&error));
    }
    if reply.projects.len() > limit as usize {
        return Err("Invalid project page size".into());
    }
    let descending = matches!(read, ProjectPage::Before(_));
    let mut last = match read {
        ProjectPage::First | ProjectPage::Only(_) => None,
        ProjectPage::After(id) | ProjectPage::Before(id) => Some(id),
    };
    for project in &reply.projects {
        let id = registry_id(project.project_id.as_deref())?;
        let ordered = last.is_none_or(|previous| {
            if descending {
                id < previous
            } else {
                id > previous
            }
        });
        let exact = match read {
            ProjectPage::Only(only) => id == only,
            _ => true,
        };
        if !valid_row(project) || !ordered || !exact {
            return Err("Invalid project registry response".into());
        }
        last = Some(id);
    }
    let next = reply
        .next_cursor
        .as_deref()
        .map(|v| registry_id(Some(v)))
        .transpose()?;
    if next.is_some()
        && (reply.projects.is_empty() || next != last || matches!(read, ProjectPage::Only(_)))
    {
        return Err("Invalid project page cursor".into());
    }
    let mut rows = reply.projects;
    if descending {
        rows.reverse();
    }
    Ok(Page { rows, next })
}

/// Reads one page through `fetch`, checking `check` before and after.
fn read_page(
    fetch: &mut impl FnMut(ProjectPage, u32) -> Result<pb::ProjectsReply, String>,
    check: &mut impl FnMut() -> Result<(), String>,
    read: ProjectPage,
    limit: u32,
) -> Result<Page, String> {
    check()?;
    let reply = fetch(read, limit)?;
    check()?;
    validate_page(read, limit, reply)
}

/// Reads one page from the service. Each read is one request with no time
/// bound; the attachment's cancel or a lost connection ends it.
pub(crate) fn project_page(
    client: &mut asura_client::Client,
    read: ProjectPage,
    limit: u32,
) -> Result<Page, String> {
    read_page(
        &mut |read, limit| client.projects(read, limit).map_err(|e| e.to_string()),
        &mut || Ok(()),
        read,
        limit,
    )
}

/// Fills a window from the first page, or after `start`, until it holds
/// [`WINDOW`] entries or no next cursor remains (CF06b).
pub(crate) fn fill_window(
    mut fetch: impl FnMut(ProjectPage, u32) -> Result<pb::ProjectsReply, String>,
    mut check: impl FnMut() -> Result<(), String>,
    start: Option<[u8; 16]>,
) -> Result<Window, String> {
    let mut read = start.map_or(ProjectPage::First, ProjectPage::After);
    let mut rows = Vec::new();
    loop {
        let limit = PAGE.min((WINDOW - rows.len()) as u32);
        let page = read_page(&mut fetch, &mut check, read, limit)?;
        rows.extend(page.rows);
        match page.next {
            Some(next) if rows.len() < WINDOW => read = ProjectPage::After(next),
            next => {
                return Ok(Window {
                    rows,
                    more_before: start.is_some(),
                    more_after: next.is_some(),
                });
            }
        }
    }
}

/// Reads every page in ascending ID order and passes each to `emit` as it
/// arrives. The list has no page total: the loop ends when no next cursor
/// remains (each cursor is strictly higher), on an error or on `check`.
/// Returns the number of entries read.
pub(crate) fn each_page(
    mut fetch: impl FnMut(ProjectPage, u32) -> Result<pb::ProjectsReply, String>,
    mut check: impl FnMut() -> Result<(), String>,
    mut emit: impl FnMut(&[pb::ProjectReply]) -> Result<(), String>,
) -> Result<usize, String> {
    let mut read = ProjectPage::First;
    let mut count = 0;
    loop {
        let page = read_page(&mut fetch, &mut check, read, PAGE)?;
        count += page.rows.len();
        emit(&page.rows)?;
        match page.next {
            Some(next) => read = ProjectPage::After(next),
            None => return Ok(count),
        }
    }
}

/// Project discovery: the first window for display, plus every current
/// project that contains the launch directory, from a read of every page.
/// Memory holds the window and the matches; matches are ancestors of the
/// launch directory, so their number is bounded by its depth.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Discovery {
    pub window: Window,
    pub matches: Vec<pb::ProjectReply>,
    pub empty: bool,
}
pub(crate) fn discover(
    fetch: impl FnMut(ProjectPage, u32) -> Result<pb::ProjectsReply, String>,
    check: impl FnMut() -> Result<(), String>,
    launch: &str,
) -> Result<Discovery, String> {
    let mut discovery = Discovery::default();
    let count = each_page(fetch, check, |rows| {
        for row in rows {
            if discovery.window.rows.len() < WINDOW {
                discovery.window.rows.push(row.clone());
            } else {
                discovery.window.more_after = true;
            }
            if contains_launch(row, launch) {
                discovery.matches.push(row.clone());
            }
        }
        Ok(())
    })?;
    discovery.empty = count == 0;
    Ok(discovery)
}
fn contains_launch(project: &pb::ProjectReply, launch: &str) -> bool {
    project.current == Some(true)
        && project
            .location
            .as_deref()
            .is_some_and(|path| std::path::Path::new(launch).starts_with(path))
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

/// Launch selection: the deepest current project containing the launch
/// directory, plus the other current matches for a hint. No match selects
/// nothing, even when only one project exists.
pub(crate) fn initial_project(
    projects: &[pb::ProjectReply],
    launch: &str,
) -> (Option<[u8; 16]>, Vec<[u8; 16]>) {
    let mut matched = projects
        .iter()
        .filter(|p| {
            p.current == Some(true)
                && p.location
                    .as_deref()
                    .is_some_and(|path| std::path::Path::new(launch).starts_with(path))
        })
        .filter_map(|p| {
            let depth = std::path::Path::new(p.location.as_deref()?)
                .components()
                .count();
            Some((depth, registry_id(p.project_id.as_deref()).ok()?))
        })
        .collect::<Vec<_>>();
    matched.sort_by_key(|m| std::cmp::Reverse(m.0));
    let mut ids = matched.into_iter().map(|(_, id)| id);
    (ids.next(), ids.collect())
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
        _ => Err(service_error(reason)),
    }
}

#[cfg(test)]
pub(crate) mod tests {
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
    /// SI-U5: the overlap reason is accepted, shown, and never auto-selected.
    #[test]
    fn overlapping_projects_are_listed_with_their_reason() {
        let overlapping = pb::ProjectReply {
            error: Some(OVERLAPS_ASURA_STATE.into()),
            ..record(1, "/Users/person", false)
        };
        let line = project_list_line(&overlapping);
        assert!(
            line.ends_with("/Users/person [unavailable: contains Asura state]"),
            "{line}"
        );
        assert!(project_list_line(&record(2, "/gone", false)).ends_with(" [stale]"));
        let page = |project: pb::ProjectReply| {
            fill_window(
                |_, _| {
                    Ok(pb::ProjectsReply {
                        projects: vec![project.clone()],
                        next_cursor: None,
                        error: None,
                    })
                },
                || Ok(()),
                None,
            )
            .map(|window| window.rows)
        };
        assert_eq!(
            page(overlapping.clone()).unwrap(),
            vec![overlapping.clone()]
        );
        assert!(
            page(pb::ProjectReply {
                current: Some(true),
                ..overlapping.clone()
            })
            .is_err()
        );
        assert!(
            page(pb::ProjectReply {
                error: Some("project_stale".into()),
                ..overlapping.clone()
            })
            .is_err()
        );
        assert_eq!(
            initial_project(&[overlapping], "/Users/person/work"),
            (None, vec![])
        );
    }
    /// Project `n` of a scripted registry; IDs ascend with `n`.
    pub(crate) fn numbered(n: u16) -> pb::ProjectReply {
        let mut id = [0u8; 16];
        id[14..].copy_from_slice(&n.to_be_bytes());
        pb::ProjectReply {
            project_id: Some(id.to_vec()),
            location: Some(format!("/work/p{n}")),
            registry_revision: Some(1),
            current: Some(true),
            ..Default::default()
        }
    }
    pub(crate) fn numbered_id(n: u16) -> [u8; 16] {
        numbered(n).project_id.unwrap().try_into().unwrap()
    }
    /// A scripted service registry of projects 1 to `count`, served with the
    /// ProjectList contract: ascending after a cursor, descending before
    /// one, exact for `only`; `next_cursor` is the last entry when more exist.
    pub(crate) fn serve(count: u16, read: ProjectPage, limit: u32) -> pb::ProjectsReply {
        let all = (1..=count).map(numbered).collect::<Vec<_>>();
        let id = |row: &pb::ProjectReply| row_id(row).unwrap();
        let matching: Vec<_> = match read {
            ProjectPage::First => all,
            ProjectPage::After(c) => all.into_iter().filter(|r| id(r) > c).collect(),
            ProjectPage::Before(c) => all.into_iter().rev().filter(|r| id(r) < c).collect(),
            ProjectPage::Only(c) => {
                return pb::ProjectsReply {
                    projects: all.into_iter().filter(|r| id(r) == c).collect(),
                    ..Default::default()
                };
            }
        };
        let more = matching.len() > limit as usize;
        let projects: Vec<_> = matching.into_iter().take(limit as usize).collect();
        pb::ProjectsReply {
            next_cursor: more.then(|| projects.last().unwrap().project_id.clone().unwrap()),
            projects,
            error: None,
        }
    }
    #[test]
    fn registry_fills_window_and_checks_cancellation() {
        let mut calls = 0;
        let window = fill_window(
            |read, limit| {
                calls += 1;
                Ok(serve(10, read, limit))
            },
            || Ok(()),
            None,
        )
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(window.rows.len(), 10);
        assert!(!window.more_before && !window.more_after);
        assert_eq!(
            fill_window(
                |_, _| panic!("cancelled fetch"),
                || Err("cancelled".into()),
                None
            )
            .unwrap_err(),
            "cancelled"
        );
    }
    /// CF06b: the window holds at most 64 entries; the `More:` hint appears
    /// exactly when a next cursor remains, naming the last entry's ID.
    #[test]
    fn window_stops_at_64_and_hints_only_when_more_remain() {
        for (count, more) in [(0, false), (8, false), (63, false), (64, false), (65, true)] {
            let window =
                fill_window(|read, limit| Ok(serve(count, read, limit)), || Ok(()), None).unwrap();
            assert_eq!(window.rows.len(), usize::from(count).min(WINDOW));
            assert_eq!(window.more_after, more, "{count}");
            let message = window.message();
            assert_eq!(
                message.contains("More: /project list after "),
                more,
                "{message}"
            );
            if more {
                assert!(message.ends_with(&format!(
                    "More: /project list after {}",
                    hex(&numbered_id(64))
                )));
            }
            if count == 0 {
                assert_eq!(message, "No projects. Use /project add PATH");
            }
        }
        // `/project list after ID` starts after the cursor with no gap.
        let window = fill_window(
            |read, limit| Ok(serve(200, read, limit)),
            || Ok(()),
            Some(numbered_id(64)),
        )
        .unwrap();
        assert_eq!(window.rows[0], numbered(65));
        assert_eq!(window.rows.last(), Some(&numbered(128)));
        assert!(window.more_before && window.more_after);
        let window = fill_window(
            |read, limit| Ok(serve(200, read, limit)),
            || Ok(()),
            Some(numbered_id(192)),
        )
        .unwrap();
        assert_eq!(window.rows.len(), 8);
        assert!(window.more_hint().is_none());
        let window = fill_window(
            |read, limit| Ok(serve(5, read, limit)),
            || Ok(()),
            Some(numbered_id(5)),
        )
        .unwrap();
        assert_eq!(window.message(), "No projects after this ID");
    }
    /// CF06b window edges: after appends and drops the front; before
    /// (descending reply, reversed) prepends and drops the back. Joins have
    /// no gap or duplicate, and the window never exceeds 64.
    #[test]
    fn window_edges_fetch_join_and_drop_the_far_page() {
        let mut window =
            fill_window(|read, limit| Ok(serve(200, read, limit)), || Ok(()), None).unwrap();
        let ids = |window: &Window| {
            window
                .rows
                .iter()
                .map(|row| row_id(row).unwrap())
                .collect::<Vec<_>>()
        };
        let contiguous = |window: &Window| {
            let ids = ids(window);
            ids.windows(2).all(|pair| {
                u16::from_be_bytes([pair[1][14], pair[1][15]])
                    == u16::from_be_bytes([pair[0][14], pair[0][15]]) + 1
            })
        };
        // Bottom edge: next pages until the registry ends.
        let mut reads = 0;
        while window.more_after {
            let read = ProjectPage::After(*ids(&window).last().unwrap());
            let page = validate_page(read, PAGE, serve(200, read, PAGE)).unwrap();
            window.join(read, page).unwrap();
            reads += 1;
            assert!(window.rows.len() <= WINDOW);
            assert!(contiguous(&window));
        }
        assert_eq!(reads, 17);
        assert_eq!(window.rows.first(), Some(&numbered(137)));
        assert_eq!(window.rows.last(), Some(&numbered(200)));
        assert!(window.more_before);
        // Top edge: the before reply is descending; it is reversed for display.
        let read = ProjectPage::Before(numbered_id(137));
        let reply = serve(200, read, PAGE);
        assert_eq!(reply.projects[0], numbered(136));
        let page = validate_page(read, PAGE, reply).unwrap();
        assert_eq!(page.rows.first(), Some(&numbered(129)));
        assert_eq!(page.next, Some(numbered_id(129)));
        window.join(read, page).unwrap();
        assert_eq!(window.rows.len(), WINDOW);
        assert_eq!(window.rows.first(), Some(&numbered(129)));
        assert_eq!(window.rows.last(), Some(&numbered(192)));
        assert!(window.more_after && window.more_before);
        assert!(contiguous(&window));
        while window.more_before {
            let read = ProjectPage::Before(ids(&window)[0]);
            let page = validate_page(read, PAGE, serve(200, read, PAGE)).unwrap();
            window.join(read, page).unwrap();
            assert!(window.rows.len() <= WINDOW && contiguous(&window));
        }
        assert_eq!(window.rows.first(), Some(&numbered(1)));
        assert_eq!(window.rows.last(), Some(&numbered(64)));
        // An empty page at an edge ends reads in that direction.
        let mut small =
            fill_window(|read, limit| Ok(serve(3, read, limit)), || Ok(()), None).unwrap();
        small.more_before = true;
        let read = ProjectPage::Before(numbered_id(1));
        let page = validate_page(read, PAGE, serve(3, read, PAGE)).unwrap();
        small.join(read, page).unwrap();
        assert!(!small.more_before);
        assert_eq!(small.rows.len(), 3);
        // A page read from a cursor that is no longer the edge is refused.
        let read = ProjectPage::After(numbered_id(2));
        let page = validate_page(read, PAGE, serve(3, read, PAGE)).unwrap();
        assert!(small.join(read, page).is_err());
    }
    /// The page contract: order per direction, cursor equal to the last
    /// entry, `only` exact with no cursor, and service errors mapped.
    #[test]
    fn pages_reject_disorder_bad_cursors_and_inexact_only() {
        let reply = |projects: Vec<pb::ProjectReply>, next: Option<u16>| pb::ProjectsReply {
            projects,
            next_cursor: next.map(|n| numbered_id(n).to_vec()),
            error: None,
        };
        let first = ProjectPage::First;
        for project in [record(0, "/a", true), record(1, "relative", true)] {
            assert!(validate_page(first, 8, reply(vec![project], None)).is_err());
        }
        // Ascending reads reject descending entries; before reads the reverse.
        let down = vec![numbered(3), numbered(2)];
        assert!(validate_page(first, 8, reply(down.clone(), None)).is_err());
        let before = ProjectPage::Before(numbered_id(4));
        assert_eq!(
            validate_page(before, 8, reply(down.clone(), Some(2)))
                .unwrap()
                .rows,
            vec![numbered(2), numbered(3)]
        );
        assert!(validate_page(ProjectPage::Before(numbered_id(3)), 8, reply(down, None)).is_err());
        assert!(
            validate_page(
                ProjectPage::After(numbered_id(2)),
                8,
                reply(vec![numbered(2)], None)
            )
            .is_err()
        );
        // Duplicate entries, a cursor other than the last entry, a cursor on
        // an empty page and an oversized page are invalid.
        assert!(validate_page(first, 8, reply(vec![numbered(1), numbered(1)], None)).is_err());
        assert!(validate_page(first, 8, reply(vec![numbered(1)], Some(2))).is_err());
        assert!(validate_page(first, 8, reply(vec![], Some(1))).is_err());
        assert!(validate_page(first, 1, reply(vec![numbered(1), numbered(2)], None)).is_err());
        // `only`: found, not found, wrong project, or a cursor.
        let only = ProjectPage::Only(numbered_id(5));
        assert_eq!(
            validate_page(only, 1, serve(10, only, 1)).unwrap().rows,
            vec![numbered(5)]
        );
        assert!(
            validate_page(only, 1, serve(4, only, 1))
                .unwrap()
                .rows
                .is_empty()
        );
        assert!(validate_page(only, 1, reply(vec![numbered(6)], None)).is_err());
        assert!(validate_page(only, 1, reply(vec![numbered(5)], Some(5))).is_err());
        let error = |code: &str| pb::ProjectsReply {
            error: Some(code.into()),
            ..Default::default()
        };
        assert_eq!(
            validate_page(first, 8, error("unavailable")).unwrap_err(),
            "unavailable"
        );
        assert_eq!(
            validate_page(first, 8, error("journal_quota")).unwrap_err(),
            JOURNAL_QUOTA_NOTICE
        );
    }
    /// CLI listing: every page, in order, as it arrives; no page total.
    #[test]
    fn each_page_reads_every_page_with_no_total() {
        let mut pages = Vec::new();
        let count = each_page(
            |read, limit| Ok(serve(200, read, limit)),
            || Ok(()),
            |rows| {
                pages.push(rows.to_vec());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(count, 200);
        assert_eq!(pages.len(), 25);
        let all = pages.concat();
        assert_eq!(all, (1..=200).map(numbered).collect::<Vec<_>>());
        // The person's cancel ends the reads after the current page.
        let reads = std::cell::Cell::new(0);
        let stopped = each_page(
            |read, limit| {
                reads.set(reads.get() + 1);
                Ok(serve(200, read, limit))
            },
            || {
                if reads.get() >= 3 {
                    Err("cancelled".into())
                } else {
                    Ok(())
                }
            },
            |_| Ok(()),
        );
        assert_eq!(stopped.unwrap_err(), "cancelled");
        assert_eq!(reads.get(), 3);
    }
    /// Discovery reads every page: a launch match beyond the window is found,
    /// while the window keeps the first 64 entries.
    #[test]
    fn discovery_finds_matches_beyond_the_window() {
        let mut deep = numbered(150);
        deep.location = Some("/work/p1/inner".into());
        let discovery = discover(
            |read, limit| {
                let mut reply = serve(200, read, limit);
                for row in &mut reply.projects {
                    if row.project_id == deep.project_id {
                        *row = deep.clone();
                    }
                }
                Ok(reply)
            },
            || Ok(()),
            "/work/p1/inner/src",
        )
        .unwrap();
        assert_eq!(discovery.window.rows.len(), WINDOW);
        assert!(discovery.window.more_after);
        assert!(!discovery.empty);
        assert_eq!(discovery.matches, vec![numbered(1), deep]);
        assert_eq!(
            initial_project(&discovery.matches, "/work/p1/inner/src"),
            (Some(numbered_id(150)), vec![numbered_id(1)])
        );
        let empty = discover(|read, limit| Ok(serve(0, read, limit)), || Ok(()), "/x").unwrap();
        assert!(empty.empty && empty.window.rows.is_empty());
    }
    #[test]
    fn journal_errors_have_fixed_notices() {
        assert_eq!(service_error("journal_quota"), JOURNAL_QUOTA_NOTICE);
        assert_eq!(service_error("journal_disk_space"), JOURNAL_DISK_NOTICE);
        assert_eq!(service_error("project_stale"), "project_stale");
        assert_eq!(
            initialization_rejected("journal_quota", &[1; 16]).unwrap_err(),
            JOURNAL_QUOTA_NOTICE
        );
    }
    #[test]
    fn selection_uses_deepest_current_match_and_reports_other_matches() {
        let projects = vec![
            record(1, "/work", true),
            record(2, "/work/sub", true),
            record(3, "/work/sub/deep", false),
        ];
        assert_eq!(
            initial_project(&projects, "/work/sub/deep/file"),
            (Some([2; 16]), vec![[1; 16]])
        );
        assert_eq!(
            initial_project(&projects, "/work/x"),
            (Some([1; 16]), vec![])
        );
        assert_eq!(initial_project(&projects, "/workspace"), (None, vec![]));
        assert_eq!(
            initial_project(
                &[record(1, "/other", true), record(2, "/stale", false)],
                "/here"
            ),
            (None, vec![])
        );
        assert_eq!(
            initial_project(&[record(1, "/here", false)], "/here"),
            (None, vec![])
        );
        assert_eq!(initial_project(&[], "/here"), (None, vec![]));
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
