//! The client's project window (CF06b project paging): at most 64
//! consecutive registry entries plus the selected project. The picker reads
//! the next or previous page at its edges through the project page slot;
//! `/project select ID` outside the window reads that one project first.
//! Nothing here performs I/O; requests go through the dispatcher.
use super::super::project_pages::{Purpose, Request, Update};
use super::*;
use crate::conversation::{ProjectPage, Window};
use asura_control::pb::ProjectReply;

#[derive(Default)]
pub(super) struct Paging {
    /// A read before the window's first entry may find more projects.
    pub more_before: bool,
    /// A read after the window's last entry may find more projects.
    pub more_after: bool,
    /// Changes whenever the window changes; an edge page joins only the
    /// window it was read for.
    pub revision: u64,
    /// The selected project's row when it is outside the window.
    pub selected_row: Option<ProjectReply>,
    request: Option<Request>,
    /// The read waiting for or holding the page slot.
    pending: Option<Purpose>,
    cancel: bool,
}

fn has_id(row: &ProjectReply, id: [u8; 16]) -> bool {
    row.project_id.as_deref() == Some(id.as_slice())
}

impl App {
    /// The row of a project in the window, or of the selected project.
    pub(super) fn project_row(&self, id: [u8; 16]) -> Option<&ProjectReply> {
        self.projects
            .as_ref()
            .and_then(|rows| rows.iter().find(|row| has_id(row, id)))
            .or_else(|| {
                self.paging
                    .selected_row
                    .as_ref()
                    .filter(|row| has_id(row, id))
            })
    }
    pub(super) fn project_row_mut(&mut self, id: [u8; 16]) -> Option<&mut ProjectReply> {
        let in_window = self
            .projects
            .as_ref()
            .is_some_and(|rows| rows.iter().any(|row| has_id(row, id)));
        if in_window {
            self.projects
                .as_mut()
                .and_then(|rows| rows.iter_mut().find(|row| has_id(row, id)))
        } else {
            self.paging
                .selected_row
                .as_mut()
                .filter(|row| has_id(row, id))
        }
    }
    pub(super) fn in_window(&self, id: [u8; 16]) -> bool {
        self.projects
            .as_ref()
            .is_some_and(|rows| rows.iter().any(|row| has_id(row, id)))
    }
    /// Replaces the window. An open picker shows the new rows and keeps its
    /// highlighted project when that project is still present.
    pub(super) fn set_project_window(&mut self, window: Window) {
        self.paging.more_before = window.more_before;
        self.paging.more_after = window.more_after;
        self.paging.revision = self.paging.revision.wrapping_add(1);
        if let Some(Picker::Projects { selected, rows }) = &mut self.picker {
            let highlighted = rows.get(*selected).and_then(|row| row.project_id.clone());
            *rows = window.rows.clone();
            *selected = rows
                .iter()
                .position(|row| row.project_id == highlighted)
                .unwrap_or((*selected).min(rows.len().saturating_sub(1)));
        }
        // The window holds the selected project besides its entries: a row
        // that leaves the window is kept as the selected row.
        if let Some(id) = self.selected_project {
            if window.rows.iter().any(|row| has_id(row, id)) {
                self.paging.selected_row = None;
            } else if let Some(row) = self
                .projects
                .as_ref()
                .and_then(|rows| rows.iter().find(|row| has_id(row, id)))
            {
                self.paging.selected_row = Some(row.clone());
            }
        }
        self.projects = Some(window.rows);
    }
    pub(in super::super) fn take_project_page_request(&mut self) -> Option<Request> {
        self.paging.request.take()
    }
    pub(in super::super) fn take_project_page_cancel(&mut self) -> bool {
        std::mem::take(&mut self.paging.cancel)
    }
    /// Withdraws a request the dispatcher has not taken, or stops the read
    /// in the page slot. A stopped read's result is dropped or reported.
    fn stop_read(&mut self) {
        if self.paging.request.take().is_some() {
            self.paging.pending = None;
        } else {
            self.paging.cancel = true;
        }
    }
    /// Stops an edge read when the picker closes; its result is dropped.
    pub(super) fn stop_edge_read(&mut self) {
        if matches!(self.paging.pending, Some(Purpose::Edge { .. })) {
            self.stop_read();
        }
    }
    /// `/cancel` or Escape: stops a pending `only` read for selection.
    pub(super) fn stop_select_read(&mut self) -> bool {
        if matches!(self.paging.pending, Some(Purpose::Select { .. })) {
            self.stop_read();
            self.notice = "Project read stopped; draft retained".into();
            return true;
        }
        false
    }
    /// After picker navigation: at the last row read the next page, at the
    /// first row the previous page, when more may exist in that direction.
    pub(super) fn request_edge_page(&mut self, selected: usize, count: usize) {
        if self.paging.pending.is_some() || count == 0 {
            return;
        }
        let Some(rows) = self.projects.as_ref() else {
            return;
        };
        let edge_id = |row: Option<&ProjectReply>| {
            row.and_then(|row| row.project_id.as_deref()?.try_into().ok())
        };
        let read = if selected + 1 == count && self.paging.more_after {
            edge_id(rows.last()).map(ProjectPage::After)
        } else if selected == 0 && self.paging.more_before {
            edge_id(rows.first()).map(ProjectPage::Before)
        } else {
            None
        };
        if let Some(read) = read {
            let purpose = Purpose::Edge {
                window_revision: self.paging.revision,
            };
            self.paging.pending = Some(purpose.clone());
            self.paging.request = Some(Request { read, purpose });
        }
    }
    /// `/project select ID` for an ID outside the window: read that one
    /// project, then apply the normal selection guard.
    pub(super) fn select_outside_window(&mut self, id: [u8; 16], draft: String) {
        if self.paging.pending.is_some() {
            self.notice = "Project read pending; draft retained".into();
            return;
        }
        let purpose = Purpose::Select { draft };
        self.paging.pending = Some(purpose.clone());
        self.paging.request = Some(Request {
            read: ProjectPage::Only(id),
            purpose,
        });
        self.notice = "Reading project; Esc stops waiting".into();
    }
    pub(in super::super) fn project_page_finished(&mut self, update: Update) {
        self.paging.pending = None;
        match update.request.purpose {
            Purpose::Edge { window_revision } => {
                // A late page after the picker closed, or for a window that
                // has since changed, is dropped.
                if window_revision != self.paging.revision
                    || !matches!(self.picker, Some(Picker::Projects { .. }))
                {
                    return;
                }
                let page = match update.result {
                    Ok(page) => page,
                    Err(error) => {
                        self.notice = format!("Project page unavailable: {}", literal(&error, 512));
                        return;
                    }
                };
                let mut window = Window {
                    rows: self.projects.clone().unwrap_or_default(),
                    more_before: self.paging.more_before,
                    more_after: self.paging.more_after,
                };
                match window.join(update.request.read, page) {
                    Ok(()) => self.set_project_window(window),
                    Err(error) => self.notice = error,
                }
            }
            Purpose::Select { draft } => {
                let row = match update.result {
                    Ok(page) => page.rows.into_iter().next(),
                    Err(error) => {
                        self.notice = format!("{}; draft retained", literal(&error, 512));
                        return;
                    }
                };
                let ProjectPage::Only(id) = update.request.read else {
                    return;
                };
                let Some(row) = row else {
                    self.notice = "Project not found; draft retained".into();
                    return;
                };
                if self.select_project_row(id, Some(row)) && self.editor.text() == draft {
                    self.editor = Editor::new();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::tests::{numbered, numbered_id, serve};
    use crate::conversation::{PAGE, validate_page};
    use crossterm::event::KeyEvent;

    fn key(app: &mut App, code: KeyCode) {
        app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }
    fn type_draft(app: &mut App, text: &str) {
        app.editor = Editor::new();
        app.editor.insert(text).unwrap();
    }
    fn window(count: u16, start: Option<u16>) -> Window {
        crate::conversation::fill_window(
            |read, limit| Ok(serve(count, read, limit)),
            || Ok(()),
            start.map(numbered_id),
        )
        .unwrap()
    }
    /// Answers the app's pending page request from a registry of `count`.
    fn answer(app: &mut App, count: u16) -> ProjectPage {
        let request = app.take_project_page_request().expect("page request");
        let limit = match request.read {
            ProjectPage::Only(_) => 1,
            _ => PAGE,
        };
        let read = request.read;
        let result = validate_page(read, limit, serve(count, read, limit));
        app.project_page_finished(Update { request, result });
        read
    }
    fn picker(app: &App) -> (usize, Vec<ProjectReply>) {
        match &app.picker {
            Some(Picker::Projects { selected, rows }) => (*selected, rows.clone()),
            _ => panic!("project picker closed"),
        }
    }
    fn open(app: &mut App) {
        app.focus = Focus::Project;
        key(app, KeyCode::Enter);
    }

    /// CF06b picker edges: the bottom edge reads the next page and drops the
    /// first; the top edge reads the previous page (descending, reversed)
    /// and drops the last. The window never exceeds 64 entries and joins
    /// have no gap or duplicate; selection reaches entry 150.
    #[test]
    fn picker_reads_pages_at_its_edges_and_keeps_64_entries() {
        let mut app = App::new();
        app.set_project_window(window(200, None));
        open(&mut app);
        assert_eq!(picker(&app).1.len(), 64);
        key(&mut app, KeyCode::End);
        assert_eq!(answer(&mut app, 200), ProjectPage::After(numbered_id(64)));
        let (selected, rows) = picker(&app);
        assert_eq!(rows.len(), 64);
        assert_eq!(rows[0], numbered(9));
        assert_eq!(rows[selected], numbered(64));
        // Walk down to project 150: each bottom-edge arrival reads a page.
        while picker(&app).1[picker(&app).0] != numbered(150) {
            key(&mut app, KeyCode::Down);
            if app.paging.request.is_some() {
                answer(&mut app, 200);
            }
            let rows = picker(&app).1;
            assert!(rows.len() <= 64);
            let ids: Vec<_> = rows.iter().map(|r| r.project_id.clone().unwrap()).collect();
            assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
        }
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.selected_project, Some(numbered_id(150)));
        assert!(app.picker.is_none());
        // The top edge reads the previous page and drops the last one.
        open(&mut app);
        let first = picker(&app).1[0].clone();
        key(&mut app, KeyCode::Home);
        let read = answer(&mut app, 200);
        assert_eq!(
            read,
            ProjectPage::Before(first.project_id.clone().unwrap().try_into().unwrap())
        );
        let (selected, rows) = picker(&app);
        assert_eq!(rows.len(), 64);
        assert_eq!(rows[selected], first);
        assert_eq!(rows[selected - 1], {
            let n = u16::from_be_bytes([
                first.project_id.as_ref().unwrap()[14],
                first.project_id.as_ref().unwrap()[15],
            ]);
            numbered(n - 1)
        });
        assert!(app.paging.more_after);
        // The selected project stays known when it leaves the window.
        assert_eq!(
            app.project_row(numbered_id(150))
                .map(|row| row.location.clone()),
            Some(numbered(150).location)
        );
    }

    /// No read at an edge with nothing beyond it; an empty page ends reads.
    #[test]
    fn edges_without_more_projects_read_nothing() {
        let mut app = App::new();
        app.set_project_window(window(10, None));
        open(&mut app);
        key(&mut app, KeyCode::End);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Home);
        key(&mut app, KeyCode::Up);
        assert!(app.take_project_page_request().is_none());
        // A window from `/project list after ID` may have entries before it.
        let mut app = App::new();
        app.set_project_window(window(10, Some(2)));
        open(&mut app);
        key(&mut app, KeyCode::Up);
        answer(&mut app, 10);
        assert_eq!(picker(&app).1[0], numbered(1));
        assert_eq!(picker(&app).1.len(), 10);
        key(&mut app, KeyCode::Home);
        assert!(app.take_project_page_request().is_none());
    }

    /// Escape drops a late edge page and stops its read; a page for a
    /// window that changed meanwhile is dropped too.
    #[test]
    fn late_or_stale_edge_pages_are_dropped() {
        let mut app = App::new();
        app.set_project_window(window(200, None));
        open(&mut app);
        key(&mut app, KeyCode::End);
        let request = app.take_project_page_request().unwrap();
        key(&mut app, KeyCode::Esc);
        assert!(app.take_project_page_cancel());
        let read = request.read;
        app.project_page_finished(Update {
            request,
            result: validate_page(read, PAGE, serve(200, read, PAGE)),
        });
        assert_eq!(app.projects.as_ref().unwrap().len(), 64);
        assert_eq!(app.projects.as_ref().unwrap()[0], numbered(1));
        open(&mut app);
        key(&mut app, KeyCode::End);
        let request = app.take_project_page_request().unwrap();
        app.set_project_window(window(200, None));
        let read = request.read;
        app.project_page_finished(Update {
            request,
            result: validate_page(read, PAGE, serve(200, read, PAGE)),
        });
        assert_eq!(picker(&app).1[0], numbered(1));
        // A read error is shown and the picker stays open.
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::End);
        let request = app.take_project_page_request().unwrap();
        app.project_page_finished(Update {
            request,
            result: Err("service_unavailable".into()),
        });
        assert!(app.notice.contains("Project page unavailable"));
        assert!(app.picker.is_some());
    }

    /// `/project select ID` outside the window reads that project with
    /// `only`: found selects it; not found and errors keep the draft.
    #[test]
    fn select_outside_the_window_reads_the_project_first() {
        let mut app = App::new();
        app.set_project_window(window(200, None));
        let draft = format!(
            "/project select {}",
            crate::conversation::hex(&numbered_id(150))
        );
        type_draft(&mut app, &draft);
        key(&mut app, KeyCode::Enter);
        assert!(app.notice.contains("Reading project"));
        assert_eq!(app.selected_project, None);
        assert_eq!(answer(&mut app, 200), ProjectPage::Only(numbered_id(150)));
        assert_eq!(app.selected_project, Some(numbered_id(150)));
        assert!(app.editor.text().is_empty());
        assert_eq!(app.project_name(numbered_id(150)), "P150");
        assert_eq!(app.projects.as_ref().unwrap().len(), 64);
        // Not found.
        let draft = format!(
            "/project select {}",
            crate::conversation::hex(&numbered_id(300))
        );
        type_draft(&mut app, &draft);
        key(&mut app, KeyCode::Enter);
        answer(&mut app, 200);
        assert_eq!(app.notice, "Project not found; draft retained");
        assert_eq!(app.editor.text(), draft);
        assert_eq!(app.selected_project, Some(numbered_id(150)));
        // Error, including a journal hold reason, keeps the draft.
        let draft = format!(
            "/project select {}",
            crate::conversation::hex(&numbered_id(170))
        );
        type_draft(&mut app, &draft);
        key(&mut app, KeyCode::Enter);
        let request = app.take_project_page_request().unwrap();
        app.project_page_finished(Update {
            request,
            result: Err("service_unavailable".into()),
        });
        assert_eq!(app.notice, "service_unavailable; draft retained");
        assert_eq!(app.editor.text(), draft);
        // A second read waits for the first; Escape stops the wait.
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        assert!(app.notice.contains("pending"));
        // Escape withdraws a read not yet dispatched, or stops one in flight.
        key(&mut app, KeyCode::Esc);
        assert!(app.take_project_page_request().is_none());
        assert!(app.notice.contains("Project read stopped"));
        assert_eq!(app.editor.text(), draft);
        key(&mut app, KeyCode::Enter);
        let request = app.take_project_page_request().unwrap();
        key(&mut app, KeyCode::Esc);
        assert!(app.take_project_page_cancel());
        app.project_page_finished(Update {
            request,
            result: Err("cancelled".into()),
        });
        assert_eq!(app.notice, "cancelled; draft retained");
        assert_eq!(app.editor.text(), draft);
        // A project in the window is selected at once with no read.
        let mut app = App::new();
        app.set_project_window(window(200, None));
        type_draft(
            &mut app,
            &format!(
                "/project select {}",
                crate::conversation::hex(&numbered_id(3))
            ),
        );
        key(&mut app, KeyCode::Enter);
        assert!(app.take_project_page_request().is_none());
        assert_eq!(app.selected_project, Some(numbered_id(3)));
    }

    /// `/project list after ID` validates ID like `/project select ID`.
    #[test]
    fn list_after_parses_and_validates_the_cursor() {
        use super::super::super::conversation::Request;
        use crate::conversation::Command;
        let id = crate::conversation::hex(&numbered_id(64));
        for (draft, expected) in [
            ("/project list".to_string(), Some(None)),
            (
                format!("/project list after {id}"),
                Some(Some(numbered_id(64))),
            ),
            (
                format!("/project list after  {id} "),
                Some(Some(numbered_id(64))),
            ),
            ("/project list after".into(), None),
            (format!("/project list after {}", "0".repeat(32)), None),
            (format!("/project list after {}", "g".repeat(32)), None),
            (format!("/project list after {}", &id[..31]), None),
            (format!("/project list before {id}"), None),
            (format!("/project list after {id} extra"), None),
        ] {
            let mut app = App::new();
            type_draft(&mut app, &draft);
            key(&mut app, KeyCode::Enter);
            let request = app.take_conversation_request();
            match expected {
                Some(after) => assert!(
                    matches!(request, Some(Request::Setup(Command::List(a))) if a == after),
                    "{draft}"
                ),
                None => {
                    assert!(request.is_none(), "{draft}");
                    assert!(!app.notice.is_empty(), "{draft}");
                    assert_eq!(app.editor.text(), draft);
                }
            }
        }
    }

    /// A `/project list` result replaces the window and its edges; the
    /// selected project outside the new window stays known.
    #[test]
    fn list_result_replaces_the_window_and_keeps_the_selected_row() {
        use super::super::super::conversation::Update as ListUpdate;
        let mut app = App::new();
        app.set_project_window(window(200, None));
        assert!(app.select_project(numbered_id(10)));
        let next = window(200, Some(64));
        app.conversation_finished(ListUpdate {
            message: Some(next.message()),
            projects: Some(next.rows.clone()),
            more_before: next.more_before,
            more_after: next.more_after,
            preserve_project_selection: true,
            success: true,
            done: true,
            ..Default::default()
        });
        assert_eq!(app.projects.as_ref().unwrap()[0], numbered(65));
        assert!(app.paging.more_before && app.paging.more_after);
        assert_eq!(app.selected_project, Some(numbered_id(10)));
        assert_eq!(app.project_name(numbered_id(10)), "P10");
        assert!(
            app.config_result
                .as_deref()
                .is_some_and(|text| text.ends_with(&format!(
                    "More: /project list after {}",
                    crate::conversation::hex(&numbered_id(128))
                )))
        );
    }

    /// A found project still passes the idle guard before selection.
    #[test]
    fn select_read_applies_the_idle_guard() {
        let mut app = App::new();
        app.set_project_window(window(200, None));
        let draft = format!(
            "/project select {}",
            crate::conversation::hex(&numbered_id(99))
        );
        type_draft(&mut app, &draft);
        key(&mut app, KeyCode::Enter);
        app.queue_busy = true;
        answer(&mut app, 200);
        assert_eq!(app.selected_project, None);
        assert!(app.notice.contains("Finish active work"));
        assert_eq!(app.editor.text(), draft);
    }
}
