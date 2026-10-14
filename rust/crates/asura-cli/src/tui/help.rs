//! Client-local help. The user guide under `docs/user/` is compiled into the
//! binary, parsed once into a bounded index, and read by `/help`, `?` search and
//! area help. Nothing here performs I/O, takes a lock or starts a worker.
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    fmt,
    ops::Range,
    sync::OnceLock,
};

macro_rules! guide_file {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../docs/user/",
            $name
        ))
    };
}
const GUIDE: &str = guide_file!("guide.md");
const COMMANDS: &str = guide_file!("commands.md");
const KEYS: &str = guide_file!("keys.md");

const MAX_EMBEDDED_BYTES: usize = 131_072;
const _: () = assert!(GUIDE.len() + COMMANDS.len() + KEYS.len() <= MAX_EMBEDDED_BYTES);

pub(super) const MAX_ENTRIES: usize = 512;
pub(super) const MAX_RESULTS: usize = 50;
const MAX_TITLE_BYTES: usize = 120;
const MAX_SUMMARY_BYTES: usize = 160;
const MAX_BODY_BYTES: usize = 16 * 1024;
const MAX_SEARCH_BODY_BYTES: usize = 2 * 1024;
const MAX_FOLDED_BYTES: usize = 160 * 1024;
const MAX_QUERY_BYTES: usize = 256;
pub(super) const MAX_TERMS: usize = 8;

/// The content that failed the format contract. Embedded content is tested, so
/// callers treat any error as "help unavailable" rather than as a panic.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ParseError {
    MissingTitle(String),
    MultipleTitles(String),
    BadTableHeader { file: String, header: String },
    BadCommandCell { file: String, cell: String },
    DuplicateId(String),
    EntryTooLarge { id: String, field: &'static str },
    TooManyEntries,
    FoldedTextTooLarge,
    UnresolvedLink { file: String, target: String },
}
impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "help content invalid: {self:?}")
    }
}
impl std::error::Error for ParseError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Command,
    Key,
    Topic,
}
impl Kind {
    pub(super) fn label(self) -> &'static str {
        match self {
            Kind::Command => "Command",
            Kind::Key => "Key",
            Kind::Topic => "Topic",
        }
    }
}

/// One searchable guide element.
#[derive(Debug)]
pub(super) struct Entry {
    pub kind: Kind,
    pub id: String,
    pub title: String,
    pub summary: String,
    pub planned: bool,
    /// Index of the file in the index's sources.
    file: usize,
    /// Topics: the section's byte range in its file. Rows: unused.
    body: Range<usize>,
    /// Topics: this entry. Rows: the topic whose section holds the row.
    section: usize,
    title_folded: String,
    labels_folded: String,
    body_folded: String,
}

#[derive(Debug)]
pub(super) struct Index<'a> {
    sources: Vec<(String, &'a str)>,
    entries: Vec<Entry>,
    by_id: HashMap<String, usize>,
}

/// Lowercase and replace whitespace, `+`, `·`, `—` and `,` with one space. No
/// Unicode normalization form or accent folding is applied.
pub(super) fn fold(text: &str) -> String {
    text.to_lowercase()
        .split(|c: char| c.is_whitespace() || matches!(c, '+' | '·' | '—' | ','))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// GitHub heading slug: lowercase, punctuation other than `-` and `_` removed,
/// each space replaced by `-`.
fn slug(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ' '))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

fn truncate(text: &str, max: usize) -> &str {
    &text[..text.floor_char_boundary(max.min(text.len()))]
}

/// A normalized search query: at most 256 input bytes, 8 distinct terms.
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub(super) struct Query {
    terms: Vec<String>,
    folded: String,
}
impl Query {
    pub(super) fn new(text: &str) -> Self {
        let folded = fold(truncate(text, MAX_QUERY_BYTES));
        let mut terms: Vec<String> = Vec::new();
        for word in folded.split(' ').filter(|w| !w.is_empty()) {
            if terms.len() == MAX_TERMS {
                break;
            }
            if !terms.iter().any(|t| t == word) {
                terms.push(word.to_owned());
            }
        }
        let folded = terms.join(" ");
        Self { terms, folded }
    }
    pub(super) fn is_blank(&self) -> bool {
        self.terms.is_empty()
    }
    #[cfg(test)]
    pub(super) fn folded(&self) -> &str {
        &self.folded
    }
    #[cfg(test)]
    pub(super) fn terms(&self) -> &[String] {
        &self.terms
    }
}

/// Bounded search result. `rows` holds at most 50 entry indexes, best first.
#[derive(Debug, Default)]
pub(super) struct Results {
    pub rows: Vec<usize>,
    /// Number of matching entries, including those beyond the first 50.
    pub total: usize,
    /// Term checks performed; at most entries times terms.
    pub work: usize,
}

/// Sort key of a result: score (high first), implemented before planned, kind, guide order.
type RankKey = (Reverse<u32>, bool, Kind, usize);

/// Fixed order of the area rows and area help.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Area {
    Box,
    Queue,
    Log,
    StatusBar,
}
impl Area {
    pub(super) const ALL: [Area; 4] = [Area::Box, Area::Queue, Area::Log, Area::StatusBar];
    pub(super) fn name(self) -> &'static str {
        match self {
            Area::Box => "Box",
            Area::Queue => "Queue",
            Area::Log => "Log",
            Area::StatusBar => "Status bar",
        }
    }
    /// Guide section then key sections, in the order the overlay joins them.
    pub(super) fn sections(self) -> &'static [&'static str] {
        match self {
            Area::Box => &[
                "guide#input-box",
                "guide#write-and-edit-text",
                "keys#input-box-keys",
                "keys#keys-anywhere",
            ],
            Area::Queue => &[
                "guide#queue-panel",
                "guide#use-the-queue",
                "keys#queue-keys",
            ],
            Area::Log => &[
                "guide#conversation-log",
                "guide#browse-responses-and-tool-activity",
                "keys#conversation-log-keys",
                "keys#response-browser-keys",
            ],
            Area::StatusBar => &[
                "guide#status-bar",
                "keys#status-bar-keys",
                "keys#project-and-model-list-keys",
            ],
        }
    }
}
/// Guide entries listed after the area rows.
pub(super) const GUIDE_ROWS: [&str; 2] = ["commands#commands-reference", "keys#keys-reference"];

/// The embedded index, built once on first use. `None` means the content failed
/// its format contract; callers show the defensive "unavailable" text.
pub(super) fn index() -> Option<&'static Index<'static>> {
    static INDEX: OnceLock<Result<Index<'static>, ParseError>> = OnceLock::new();
    INDEX
        .get_or_init(|| Index::build(&embedded()))
        .as_ref()
        .ok()
}
/// The compiled-in sources as `(file stem, Markdown)`; `README.md` is only an index.
pub(super) fn embedded() -> [(&'static str, &'static str); 3] {
    [("guide", GUIDE), ("commands", COMMANDS), ("keys", KEYS)]
}
pub(super) const UNAVAILABLE: &str = "Help content unavailable in this build";

#[derive(Default)]
struct Cell {
    text: String,
    code: Vec<String>,
    other_text: bool,
}
impl Cell {
    /// The cell's text when it is exactly one code span.
    fn single_code(&self) -> Option<&str> {
        (self.code.len() == 1 && !self.other_text).then(|| self.code[0].as_str())
    }
}
#[derive(Clone, Copy, PartialEq)]
enum TableKind {
    Other,
    Command,
    Key,
}
struct Table {
    kind: TableKind,
    in_head: bool,
    head: Vec<String>,
    row: Vec<Cell>,
    cell: Option<Cell>,
}
struct Open {
    index: usize,
    /// Text for search, including table cells.
    plain: String,
    /// Text outside tables, for the summary.
    lead: String,
}
impl Open {
    fn text(&mut self, text: &str) {
        if self.plain.len() < 2 * MAX_SEARCH_BODY_BYTES {
            self.plain.push_str(text);
        }
        if self.lead.len() < 2 * MAX_SUMMARY_BYTES {
            self.lead.push_str(text);
        }
    }
}

struct Builder {
    entries: Vec<Entry>,
    ids: HashSet<String>,
    links: Vec<(usize, String)>,
    folded: usize,
}
impl Builder {
    fn push(&mut self, entry: Entry) -> Result<usize, ParseError> {
        if self.entries.len() >= MAX_ENTRIES {
            return Err(ParseError::TooManyEntries);
        }
        if !self.ids.insert(entry.id.clone()) {
            return Err(ParseError::DuplicateId(entry.id));
        }
        if entry.title.len() > MAX_TITLE_BYTES {
            return Err(ParseError::EntryTooLarge {
                id: entry.id,
                field: "title",
            });
        }
        self.folded +=
            entry.title_folded.len() + entry.labels_folded.len() + entry.body_folded.len();
        if self.folded > MAX_FOLDED_BYTES {
            return Err(ParseError::FoldedTextTooLarge);
        }
        self.entries.push(entry);
        Ok(self.entries.len() - 1)
    }
    /// Complete the open topic: its body runs to `end`.
    fn close(&mut self, open: Open, end: usize) -> Result<(), ParseError> {
        let entry = &mut self.entries[open.index];
        entry.body.end = end;
        if entry.body.len() > MAX_BODY_BYTES {
            return Err(ParseError::EntryTooLarge {
                id: entry.id.clone(),
                field: "body",
            });
        }
        let plain = open.plain.split_whitespace().collect::<Vec<_>>().join(" ");
        let lead = open.lead.split_whitespace().collect::<Vec<_>>().join(" ");
        let sentence = lead.find(". ").map_or(lead.as_str(), |at| &lead[..=at]);
        entry.summary = truncate(sentence, MAX_SUMMARY_BYTES).to_owned();
        let body = fold(truncate(&plain, 2 * MAX_SEARCH_BODY_BYTES));
        entry.body_folded = truncate(&body, MAX_SEARCH_BODY_BYTES).to_owned();
        self.folded += entry.body_folded.len();
        if self.folded > MAX_FOLDED_BYTES {
            return Err(ParseError::FoldedTextTooLarge);
        }
        Ok(())
    }
}

fn labels(kind: Kind, area: &str, planned: bool) -> String {
    fold(&format!(
        "{} {area}{}",
        kind.label(),
        if planned { " planned" } else { "" }
    ))
}

/// Key table area name: the section heading without a trailing "keys".
fn area_name(heading: &str) -> String {
    let lower = heading.to_lowercase();
    let mut name = heading;
    if lower.ends_with(" keys") {
        name = &heading[..heading.len() - 5];
    } else if lower.starts_with("keys ") {
        name = &heading[5..];
    }
    let mut chars = name.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn parse_file(
    builder: &mut Builder,
    stem: &str,
    file: usize,
    source: &str,
) -> Result<(), ParseError> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    let mut heading: Option<(HeadingLevel, usize, String)> = None;
    let mut open: Option<Open> = None;
    let mut table: Option<Table> = None;
    let mut titles = 0;
    let mut seen_block = false;
    let mut section_slug = String::new();
    let mut section_title = String::new();
    let mut section_index = 0;
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        if !seen_block
            && !matches!(
                event,
                Event::Start(Tag::Heading {
                    level: HeadingLevel::H1,
                    ..
                })
            )
        {
            return Err(ParseError::MissingTitle(stem.to_owned()));
        }
        seen_block = true;
        match event {
            Event::Start(Tag::Heading { level, .. }) if level <= HeadingLevel::H3 => {
                if level == HeadingLevel::H1 {
                    titles += 1;
                    if titles > 1 {
                        return Err(ParseError::MultipleTitles(stem.to_owned()));
                    }
                }
                if let Some(topic) = open.take() {
                    builder.close(topic, range.start)?;
                }
                heading = Some((level, range.start, String::new()));
            }
            Event::End(TagEnd::Heading(level)) if level <= HeadingLevel::H3 => {
                let Some((_, start, title)) = heading.take() else {
                    continue;
                };
                let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
                let planned = title.contains("(Planned)");
                let shown = title.replace("(Planned)", "");
                let shown = shown.trim().to_owned();
                section_slug = slug(&title);
                section_title = shown.clone();
                let index = builder.push(Entry {
                    kind: Kind::Topic,
                    id: format!("{stem}#{section_slug}"),
                    labels_folded: labels(Kind::Topic, stem, planned),
                    title_folded: fold(&shown),
                    title: shown,
                    summary: String::new(),
                    planned,
                    file,
                    body: start..source.len(),
                    section: builder.entries.len(),
                    body_folded: String::new(),
                })?;
                section_index = index;
                open = Some(Open {
                    index,
                    plain: String::new(),
                    lead: String::new(),
                });
            }
            Event::Start(Tag::Heading { .. }) => {}
            Event::Text(text) | Event::Code(text) | Event::Html(text) | Event::InlineHtml(text)
                if heading.is_some() =>
            {
                if let Some((_, _, title)) = &mut heading {
                    title.push_str(&text);
                }
            }
            Event::Start(Tag::Table(_)) => {
                table = Some(Table {
                    kind: TableKind::Other,
                    in_head: false,
                    head: Vec::new(),
                    row: Vec::new(),
                    cell: None,
                });
            }
            Event::Start(Tag::TableHead) => {
                if let Some(t) = &mut table {
                    t.in_head = true;
                    t.row.clear();
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(t) = &mut table {
                    t.in_head = false;
                    t.head = t.row.drain(..).map(|c| c.text.trim().to_owned()).collect();
                    let first = t.head.first().map(String::as_str);
                    let second = t.head.get(1).map(String::as_str);
                    t.kind = match (first, second) {
                        (Some("Command"), Some("Action")) if t.head.len() == 2 => {
                            TableKind::Command
                        }
                        (Some("Key"), Some("Action")) if t.head.len() == 2 => TableKind::Key,
                        (Some("Command" | "Key"), _) => {
                            return Err(ParseError::BadTableHeader {
                                file: stem.to_owned(),
                                header: t.head.join(" | "),
                            });
                        }
                        _ => TableKind::Other,
                    };
                }
            }
            Event::Start(Tag::TableRow) => {
                if let Some(t) = &mut table {
                    t.row.clear();
                }
            }
            Event::Start(Tag::TableCell) => {
                if let Some(t) = &mut table {
                    t.cell = Some(Cell::default());
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(t) = &mut table
                    && let Some(cell) = t.cell.take()
                {
                    if let Some(topic) = &mut open {
                        topic.plain.push_str(&cell.text);
                        topic.plain.push(' ');
                    }
                    t.row.push(cell);
                }
            }
            Event::End(TagEnd::TableRow) => {
                let Some(t) = &mut table else { continue };
                let cells = std::mem::take(&mut t.row);
                let kind = t.kind;
                if kind == TableKind::Other {
                    continue;
                }
                let action = cells.get(1).map_or("", |c| c.text.trim()).to_owned();
                let first = cells
                    .first()
                    .map_or_else(String::new, |cell| cell.text.trim().to_owned());
                let (entry_kind, id, title, area) = if kind == TableKind::Command {
                    let Some(syntax) = cells
                        .first()
                        .and_then(Cell::single_code)
                        .filter(|code| code.starts_with('/'))
                    else {
                        return Err(ParseError::BadCommandCell {
                            file: stem.to_owned(),
                            cell: first,
                        });
                    };
                    (
                        Kind::Command,
                        format!("command:{syntax}"),
                        syntax.to_owned(),
                        section_title.clone(),
                    )
                } else {
                    let area = area_name(&section_title);
                    (
                        Kind::Key,
                        format!("key:{section_slug}:{first}"),
                        format!("{area}: {first}"),
                        area,
                    )
                };
                builder.push(Entry {
                    kind: entry_kind,
                    id,
                    labels_folded: labels(entry_kind, &area, false),
                    title_folded: fold(&title),
                    title,
                    summary: truncate(&action, MAX_SUMMARY_BYTES).to_owned(),
                    planned: false,
                    file,
                    body: 0..0,
                    section: section_index,
                    body_folded: fold(truncate(&action, MAX_SEARCH_BODY_BYTES)),
                })?;
            }
            Event::End(TagEnd::Table) => table = None,
            Event::Text(text) => {
                if let Some(cell) = table.as_mut().and_then(|t| t.cell.as_mut()) {
                    cell.text.push_str(&text);
                    cell.other_text |= !text.trim().is_empty();
                } else if let Some(topic) = &mut open {
                    topic.text(&text);
                }
            }
            Event::Code(text) => {
                if let Some(cell) = table.as_mut().and_then(|t| t.cell.as_mut()) {
                    cell.text.push_str(&text);
                    cell.code.push(text.to_string());
                } else if let Some(topic) = &mut open {
                    topic.text(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(cell) = table.as_mut().and_then(|t| t.cell.as_mut()) {
                    cell.text.push(' ');
                    cell.other_text = true;
                } else if let Some(topic) = &mut open {
                    topic.text(" ");
                }
            }
            Event::End(
                TagEnd::Paragraph | TagEnd::Item | TagEnd::CodeBlock | TagEnd::Heading(_),
            ) => {
                if let Some(topic) = &mut open {
                    topic.text(" ");
                }
            }
            Event::Start(Tag::Link { dest_url, .. })
                if !dest_url.contains("://") && !dest_url.starts_with("mailto:") =>
            {
                builder.links.push((file, dest_url.to_string()));
            }
            _ => {}
        }
    }
    if titles == 0 {
        return Err(ParseError::MissingTitle(stem.to_owned()));
    }
    if let Some(topic) = open.take() {
        builder.close(topic, source.len())?;
    }
    Ok(())
}

impl<'a> Index<'a> {
    /// Parse the given `(file stem, Markdown)` pairs under the format contract.
    pub(super) fn build(files: &[(&str, &'a str)]) -> Result<Self, ParseError> {
        let mut builder = Builder {
            entries: Vec::new(),
            ids: HashSet::new(),
            links: Vec::new(),
            folded: 0,
        };
        for (file, (stem, source)) in files.iter().enumerate() {
            parse_file(&mut builder, stem, file, source)?;
        }
        let by_id: HashMap<String, usize> = builder
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id.clone(), index))
            .collect();
        let sources: Vec<(String, &'a str)> = files
            .iter()
            .map(|(stem, source)| ((*stem).to_owned(), *source))
            .collect();
        for (file, target) in &builder.links {
            let (path, fragment) = target.split_once('#').unwrap_or((target, ""));
            let stem = if path.is_empty() {
                Some(sources[*file].0.as_str())
            } else {
                path.strip_suffix(".md")
                    .filter(|stem| sources.iter().any(|(known, _)| known == stem))
            };
            let resolves = stem.is_some_and(|stem| {
                if fragment.is_empty() {
                    builder
                        .entries
                        .iter()
                        .any(|e| e.kind == Kind::Topic && e.id.starts_with(&format!("{stem}#")))
                } else {
                    by_id.contains_key(&format!("{stem}#{fragment}"))
                }
            });
            if !resolves {
                return Err(ParseError::UnresolvedLink {
                    file: sources[*file].0.clone(),
                    target: target.clone(),
                });
            }
        }
        Ok(Self {
            sources,
            entries: builder.entries,
            by_id,
        })
    }

    #[cfg(test)]
    pub(super) fn entries(&self) -> &[Entry] {
        &self.entries
    }
    pub(super) fn entry(&self, index: usize) -> Option<&Entry> {
        self.entries.get(index)
    }
    pub(super) fn find(&self, id: &str) -> Option<usize> {
        self.by_id.get(id).copied()
    }
    /// Title of the guide element with this ID, for fixed rows.
    pub(super) fn title(&self, id: &str) -> Option<&str> {
        self.find(id).map(|i| self.entries[i].title.as_str())
    }

    fn section_markdown(&self, index: usize) -> &'a str {
        let section = &self.entries[self.entries[index].section];
        let source = self.sources[section.file].1;
        &source[section.body.clone()]
    }

    /// Markdown for an opened entry. Rows put their own text first and then
    /// show their section; only this entry's text is copied.
    pub(super) fn markdown(&self, index: usize) -> Option<String> {
        let entry = self.entries.get(index)?;
        let section = self.section_markdown(index);
        Some(match entry.kind {
            Kind::Topic => section.to_owned(),
            Kind::Command | Kind::Key => {
                format!(
                    "**{}**\n\n{}\n\n---\n\n{section}",
                    entry.title, entry.summary
                )
            }
        })
    }

    /// The area's guide and key sections joined in the design's order.
    pub(super) fn area_markdown(&self, area: Area) -> Option<String> {
        let mut parts = Vec::new();
        for id in area.sections() {
            parts.push(self.section_markdown(self.find(id)?));
        }
        Some(parts.join("\n\n"))
    }

    /// Rows of the `/help` table: commands in guide order, then keys.
    pub(super) fn table_rows(&self) -> Vec<(&str, &str)> {
        [Kind::Command, Kind::Key]
            .into_iter()
            .flat_map(|kind| {
                self.entries
                    .iter()
                    .filter(move |e| e.kind == kind)
                    .map(|e| (e.title.as_str(), e.summary.as_str()))
            })
            .collect()
    }

    pub(super) fn search(&self, query: &Query) -> Results {
        let mut results = Results::default();
        let mut best: Vec<(RankKey, usize)> = Vec::new();
        'entries: for (index, entry) in self.entries.iter().enumerate() {
            let mut score = 0;
            for term in &query.terms {
                results.work += 1;
                let term = term.as_str();
                score += if starts_a_word(&entry.title_folded, term) {
                    8
                } else if entry.title_folded.contains(term) {
                    5
                } else if entry.labels_folded.contains(term) {
                    3
                } else if entry.body_folded.contains(term) {
                    1
                } else {
                    continue 'entries;
                };
            }
            if entry.title_folded.starts_with(&query.folded) {
                score += 4;
            }
            results.total += 1;
            let key = (Reverse(score), entry.planned, entry.kind, index);
            let at = best.partition_point(|(have, _)| have < &key);
            if at < MAX_RESULTS {
                best.insert(at, (key, index));
                best.truncate(MAX_RESULTS);
            }
        }
        results.rows = best.into_iter().map(|(_, index)| index).collect();
        results
    }
}

impl PartialOrd for Kind {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Kind {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let rank = |kind: &Kind| match kind {
            Kind::Command => 0,
            Kind::Key => 1,
            Kind::Topic => 2,
        };
        rank(self).cmp(&rank(other))
    }
}
fn starts_a_word(title: &str, term: &str) -> bool {
    title.match_indices(term).any(|(at, _)| {
        title[..at]
            .chars()
            .next_back()
            .is_none_or(|before| !before.is_alphanumeric())
    })
}

#[cfg(test)]
mod tests;
