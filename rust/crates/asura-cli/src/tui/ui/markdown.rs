//! Bounded, presentation-only Markdown projection for model responses.
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span, Text},
};

const MAX_SOURCE_BYTES: usize = 61_504; // Wire text plus the local incomplete marker.
const MAX_PRESENTATION_BYTES: usize = 65_536;
const MAX_LINES: usize = 8_192;
const MAX_SPANS: usize = 16_384;

#[derive(Default)]
struct List {
    next: Option<u64>,
}

#[derive(Default)]
struct Renderer {
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    lists: Vec<List>,
    links: Vec<String>,
    strong: usize,
    emphasis: usize,
    strike: usize,
    heading: bool,
    table_head: bool,
    table_cell: usize,
    quote_depth: usize,
    code_block: bool,
    emitted_bytes: usize,
    emitted_spans: usize,
    too_large: bool,
}

impl Renderer {
    fn style(&self) -> Style {
        let mut style = Style::default();
        if self.strong > 0 || self.heading || self.table_head {
            style = style.bold();
        }
        if self.emphasis > 0 {
            style = style.italic();
        }
        if self.strike > 0 {
            style = style.crossed_out();
        }
        if !self.links.is_empty() {
            style = style.underlined().fg(Color::Rgb(143, 211, 244));
        }
        if self.heading {
            style = style.fg(Color::Rgb(245, 248, 250));
        }
        if self.code_block {
            style = style
                .fg(Color::Rgb(196, 219, 232))
                .bg(Color::Rgb(43, 51, 60));
        }
        style
    }

    fn span(&mut self, value: &str, style: Style) {
        if value.is_empty() || self.too_large {
            return;
        }
        self.emitted_bytes = self.emitted_bytes.saturating_add(value.len());
        self.emitted_spans += 1;
        if self.emitted_bytes > MAX_PRESENTATION_BYTES || self.emitted_spans > MAX_SPANS {
            self.too_large = true;
            return;
        }
        self.spans.push(Span::styled(value.to_owned(), style));
    }

    fn prefix(&mut self) {
        if !self.spans.is_empty() {
            return;
        }
        if self.quote_depth > 0 {
            let guide = "│ ".repeat(self.quote_depth.min(8));
            self.span(&guide, Style::default().fg(Color::Rgb(139, 161, 177)));
        }
        if self.code_block {
            self.span("  ", Style::default().bg(Color::Rgb(43, 51, 60)));
        }
    }

    fn push(&mut self, value: &str, style: Style) {
        for (index, part) in value.split('\n').enumerate() {
            if index > 0 {
                self.finish_line();
            }
            if !part.is_empty() {
                self.prefix();
                self.span(part, style);
            }
            if self.too_large {
                break;
            }
        }
    }

    fn finish_line(&mut self) {
        if self.lines.len() >= MAX_LINES {
            self.too_large = true;
            return;
        }
        self.lines.push(Line::from(std::mem::take(&mut self.spans)));
    }

    fn block_break(&mut self) {
        if !self.spans.is_empty() {
            self.finish_line();
        }
        if self.lines.last().is_some_and(|line| !line.spans.is_empty()) {
            self.finish_line();
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.lists.is_empty() && self.quote_depth == 0 {
                    self.block_break();
                }
            }
            Tag::Heading { .. } => {
                self.block_break();
                self.heading = true;
            }
            Tag::Strong => self.strong += 1,
            Tag::Emphasis => self.emphasis += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.links.push(dest_url.into_string());
            }
            Tag::CodeBlock(_) => {
                self.block_break();
                self.code_block = true;
            }
            Tag::BlockQuote(_) => {
                if self.quote_depth == 0 {
                    self.block_break();
                }
                self.quote_depth += 1;
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.block_break();
                } else if !self.spans.is_empty() {
                    self.finish_line();
                }
                self.lists.push(List { next: start });
            }
            Tag::Item => {
                if !self.spans.is_empty() {
                    self.finish_line();
                }
                let depth = self.lists.len().saturating_sub(1).min(8);
                let marker = if let Some(list) = self.lists.last_mut() {
                    if let Some(next) = &mut list.next {
                        let marker = format!("{next}. ");
                        *next = next.saturating_add(1);
                        marker
                    } else {
                        "• ".to_owned()
                    }
                } else {
                    "• ".to_owned()
                };
                self.push(&format!("{}{marker}", "  ".repeat(depth)), Style::default());
            }
            Tag::Table(_) => self.block_break(),
            Tag::TableHead => self.table_head = true,
            Tag::TableRow => {
                if !self.spans.is_empty() {
                    self.finish_line();
                }
                self.table_cell = 0;
            }
            Tag::TableCell => {
                if self.table_cell > 0 {
                    self.push(" │ ", Style::default().fg(Color::Rgb(139, 161, 177)));
                }
                self.table_cell += 1;
            }
            Tag::HtmlBlock => self.block_break(),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Item | TagEnd::TableRow => {
                if !self.spans.is_empty() {
                    self.finish_line();
                }
            }
            TagEnd::Heading(_) => {
                if !self.spans.is_empty() {
                    self.finish_line();
                }
                self.heading = false;
            }
            TagEnd::CodeBlock => {
                if !self.spans.is_empty() {
                    self.finish_line();
                }
                self.code_block = false;
            }
            TagEnd::BlockQuote(_) => {
                if !self.spans.is_empty() {
                    self.finish_line();
                }
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link | TagEnd::Image => {
                if let Some(destination) = self.links.pop()
                    && !destination.is_empty()
                {
                    self.push(
                        &format!(" ({destination})"),
                        Style::default().fg(Color::Rgb(139, 161, 177)),
                    );
                }
            }
            TagEnd::List(_) => {
                if !self.spans.is_empty() {
                    self.finish_line();
                }
                self.lists.pop();
            }
            TagEnd::TableHead => self.table_head = false,
            _ => {}
        }
    }
}

/// Called on a validated response event, outside the terminal draw loop.
pub(super) fn render(source: &str) -> Text<'static> {
    if source.len() > MAX_SOURCE_BYTES {
        return Text::raw(source.to_owned());
    }
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    let mut view = Renderer::default();
    for event in Parser::new_ext(source, options) {
        match event {
            Event::Start(tag) => view.start(tag),
            Event::End(tag) => view.end(tag),
            Event::Text(value) | Event::Html(value) | Event::InlineHtml(value) => {
                let style = view.style();
                view.push(&value, style);
            }
            Event::Code(value) => view.push(
                &value,
                Style::default()
                    .fg(Color::Rgb(196, 219, 232))
                    .bg(Color::Rgb(43, 51, 60)),
            ),
            Event::SoftBreak => view.push(" ", view.style()),
            Event::HardBreak => view.finish_line(),
            Event::Rule => {
                view.block_break();
                view.push("────", Style::default().fg(Color::Rgb(139, 161, 177)));
                view.finish_line();
            }
            Event::TaskListMarker(done) => view.push(
                if done { "☑ " } else { "☐ " },
                Style::default().fg(Color::Rgb(139, 161, 177)),
            ),
            Event::FootnoteReference(value) => view.push(&format!("[^{value}]"), view.style()),
            Event::InlineMath(value) => view.push(&format!("${value}$"), view.style()),
            Event::DisplayMath(value) => view.push(&format!("$${value}$$"), view.style()),
        }
        if view.too_large {
            return Text::raw(source.to_owned());
        }
    }
    if !view.spans.is_empty() {
        view.finish_line();
    }
    if view.too_large {
        return Text::raw(source.to_owned());
    }
    while view.lines.last().is_some_and(|line| line.spans.is_empty()) {
        view.lines.pop();
    }
    if view.lines.is_empty() {
        Text::raw("")
    } else {
        Text::from(view.lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    fn plain(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn emphasis_removes_markers_and_keeps_nested_styles() {
        let text = render("Hi **bold *and italic***, ~~old~~ and `code`; \\*\\*literal\\*\\*.");
        assert_eq!(
            plain(&text),
            "Hi bold and italic, old and code; **literal**."
        );
        let line = &text.lines[0];
        let bold = line
            .spans
            .iter()
            .find(|span| span.content == "bold ")
            .unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let both = line
            .spans
            .iter()
            .find(|span| span.content == "and italic")
            .unwrap();
        assert!(
            both.style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::ITALIC)
        );
        let strike = line
            .spans
            .iter()
            .find(|span| span.content == "old")
            .unwrap();
        assert!(strike.style.add_modifier.contains(Modifier::CROSSED_OUT));
        let code = line
            .spans
            .iter()
            .find(|span| span.content == "code")
            .unwrap();
        assert_eq!(code.style.bg, Some(Color::Rgb(43, 51, 60)));
    }

    #[test]
    fn blocks_links_html_and_unfinished_markdown_remain_readable() {
        let text = render(
            "# Heading\n\n- first\n- [x] second\n\n[docs](https://example.org) <tag>\n\n```rust\nlet x = 1;\n```",
        );
        let visible = plain(&text);
        for expected in [
            "Heading",
            "• first",
            "☑ second",
            "docs (https://example.org)",
            "<tag>",
            "let x = 1;",
        ] {
            assert!(visible.contains(expected), "missing {expected}: {visible}");
        }
        assert!(
            text.lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(plain(&render("An **unfinished")), "An **unfinished");
        assert_eq!(plain(&render("你好 **世界**")), "你好 世界");
    }

    #[test]
    fn repeated_reference_expansion_falls_back_without_losing_source() {
        let destination = "x".repeat(1_000);
        let source = format!("{}\n\n[r]: https://{destination}", "[x][r] ".repeat(200));
        assert!(source.len() < MAX_SOURCE_BYTES);
        let visible = plain(&render(&source));
        assert!(visible.starts_with("[x][r]"));
        assert_eq!(visible, source);
    }
}
