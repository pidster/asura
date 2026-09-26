//! Entry-anchored transcript scrolling, independent of display retention.

use ratatui::{
    text::Line,
    widgets::{Paragraph, Wrap},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Upper,
    Text,
    Lower,
    Separator,
}

#[derive(Debug)]
pub struct LayoutLine<'a> {
    pub kind: LineKind,
    pub text: &'a str,
    pub rows: usize,
}

/// One entry's exact logical lines, including decorations. Painting and
/// scrolling share these measurements rather than estimating different widths.
#[derive(Debug)]
pub struct EntryLayout<'a> {
    pub width: u16,
    pub lines: Vec<LayoutLine<'a>>,
}

impl<'a> EntryLayout<'a> {
    pub fn new(text: &'a str, width: u16, padded: bool, separated: bool) -> Self {
        let decoration = |kind| LayoutLine {
            kind,
            text: "",
            rows: 1,
        };
        let mut lines = Vec::new();
        if padded {
            lines.push(decoration(LineKind::Upper));
        }
        lines.extend(text.lines().map(|text| LayoutLine {
            kind: LineKind::Text,
            text,
            rows: wrapped_rows(text, width),
        }));
        if padded {
            lines.push(decoration(LineKind::Lower));
        }
        if separated {
            lines.push(decoration(LineKind::Separator));
        }
        if lines.is_empty() {
            lines.push(decoration(LineKind::Text));
        }
        Self { width, lines }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollPlan {
    pub start_entry: usize,
    pub line_offset: usize,
    pub row_offset: u16,
    pub reading: bool,
    pub anchor_evicted: bool,
}

#[derive(Clone, Copy, Debug)]
struct Anchor {
    entry_id: u128,
    row: usize,
}

#[derive(Debug, Default)]
pub struct TranscriptViewport {
    anchor: Option<Anchor>,
    pending_rows: i64,
}

impl TranscriptViewport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue relative movement until the next layout has current wrapping.
    /// Negative moves toward earlier output; positive moves toward newer output.
    pub fn scroll_rows(&mut self, rows: i32) {
        self.pending_rows = self.pending_rows.saturating_add(i64::from(rows));
    }

    /// Reserve reading-position controls before layout applies queued movement.
    pub fn is_reading(&self) -> bool {
        self.anchor.is_some() || self.pending_rows < 0
    }

    /// Following is explicit; reaching the bottom by scrolling retains reading
    /// mode so later output cannot unexpectedly move the visible entry.
    pub fn follow_latest(&mut self) {
        self.anchor = None;
        self.pending_rows = 0;
    }

    pub fn plan(&mut self, entries: &[EntryLayout<'_>], base_id: u128, height: u16) -> ScrollPlan {
        if entries.iter().any(|entry| entry.width < 26) || height < 3 || entries.is_empty() {
            return ScrollPlan {
                start_entry: entries.len(),
                reading: self.anchor.is_some(),
                ..ScrollPlan::default()
            };
        }

        // Each logical line uses the exact Paragraph wrapping used by the UI.
        // Keeping these counts also lets a large entry skip complete source
        // lines before applying Paragraph's remaining u16 scroll offset.
        let rows: Vec<Vec<usize>> = entries
            .iter()
            .map(|entry| entry.lines.iter().map(|line| line.rows).collect())
            .collect();
        let counts: Vec<usize> = rows
            .iter()
            .map(|lines| {
                lines
                    .iter()
                    .fold(0usize, |sum, rows| sum.saturating_add(*rows))
            })
            .collect();
        let total = counts
            .iter()
            .fold(0usize, |sum, rows| sum.saturating_add(*rows));
        let latest = total.saturating_sub(usize::from(height));

        let mut anchor_evicted = false;
        let current = match self.anchor {
            None => latest,
            Some(anchor) => {
                let index = anchor
                    .entry_id
                    .checked_sub(base_id)
                    .and_then(|index| usize::try_from(index).ok())
                    .filter(|&index| index < entries.len());
                match index {
                    Some(index) => counts[..index]
                        .iter()
                        .fold(0usize, |sum, rows| sum.saturating_add(*rows))
                        .saturating_add(anchor.row.min(counts[index].saturating_sub(1))),
                    None => {
                        anchor_evicted = true;
                        0
                    }
                }
            }
        };

        let movement = std::mem::take(&mut self.pending_rows);
        let distance = usize::try_from(movement.unsigned_abs()).unwrap_or(usize::MAX);
        let top = if movement < 0 {
            current.saturating_sub(distance)
        } else if movement > 0 {
            // A resize may leave a retained anchor below the last full page.
            // Moving toward newer output must not unexpectedly move it upward.
            current.saturating_add(distance).min(latest.max(current))
        } else {
            current
        };

        let (start_entry, entry_row) = locate(&counts, top);
        if self.anchor.is_some() || movement < 0 {
            self.anchor = Some(Anchor {
                entry_id: base_id.saturating_add(start_entry as u128),
                row: entry_row,
            });
        }
        let (line_offset, row_offset) = locate(&rows[start_entry], entry_row);
        // The packet bounds one logical entry line to a draft plus its prefix;
        // at width >= 26 its wrapped row count fits comfortably inside u16.
        let row_offset = u16::try_from(row_offset)
            .expect("a bounded transcript logical line at width >= 26 fits u16 rows");
        ScrollPlan {
            start_entry,
            line_offset,
            row_offset,
            reading: self.anchor.is_some(),
            anchor_evicted,
        }
    }
}

fn wrapped_rows(line: &str, width: u16) -> usize {
    if line.is_empty() {
        1
    } else {
        Paragraph::new(Line::from(line))
            .wrap(Wrap { trim: false })
            .line_count(width)
            .max(1)
    }
}

fn locate(counts: &[usize], mut row: usize) -> (usize, usize) {
    for (index, count) in counts.iter().copied().enumerate() {
        if row < count {
            return (index, row);
        }
        row = row.saturating_sub(count);
    }
    // Every entry has at least one measured line. This fallback
    // keeps arithmetic bounded if a future caller supplies a saturated total.
    let last = counts.len().saturating_sub(1);
    (
        last,
        counts.get(last).copied().unwrap_or(1).saturating_sub(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};

    impl TranscriptViewport {
        fn plain_plan(
            &mut self,
            entries: &[String],
            base: u128,
            width: u16,
            height: u16,
        ) -> ScrollPlan {
            let layouts: Vec<_> = entries
                .iter()
                .map(|entry| EntryLayout::new(entry, width, false, true))
                .collect();
            self.plan(&layouts, base, height)
        }
    }

    fn entries(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("entry {index}")).collect()
    }

    fn render(entries: &[String], plan: ScrollPlan, width: u16, height: u16) -> Buffer {
        let lines: Vec<Line<'_>> = entries
            .iter()
            .skip(plan.start_entry)
            .enumerate()
            .flat_map(|(index, entry)| {
                entry
                    .lines()
                    .chain(std::iter::once(""))
                    .skip(if index == 0 { plan.line_offset } else { 0 })
            })
            .map(Line::from)
            .collect();
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((plan.row_offset, 0))
            .render(area, &mut buffer);
        buffer
    }

    fn first_line(buffer: &Buffer) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    #[test]
    fn tp4_prefix_eviction_and_append_preserve_surviving_anchor() {
        let original = entries(6);
        let mut viewport = TranscriptViewport::new();
        viewport.scroll_rows(-4);
        let before = viewport.plain_plan(&original, 100, 40, 4);
        assert_eq!(first_line(&render(&original, before, 40, 4)), "entry 2");
        let mut replaced = original[1..].to_vec();
        replaced.push("new entry".into());
        let after = viewport.plain_plan(&replaced, 101, 40, 4);
        assert_eq!(after.start_entry, before.start_entry - 1);
        assert!(after.reading);
        assert!(!after.anchor_evicted);
        assert_eq!(
            render(&original, before, 40, 4),
            render(&replaced, after, 40, 4)
        );
    }

    #[test]
    fn tp4_evicted_anchor_moves_to_first_retained_and_reports_once() {
        let original = entries(6);
        let mut viewport = TranscriptViewport::new();
        viewport.scroll_rows(-100);
        viewport.plain_plan(&original, 100, 40, 4);
        let retained = &original[2..];
        let lost = viewport.plain_plan(retained, 102, 40, 4);
        assert!(lost.anchor_evicted);
        assert!(lost.reading);
        assert_eq!(
            (lost.start_entry, lost.line_offset, lost.row_offset),
            (0, 0, 0)
        );
        assert_eq!(first_line(&render(retained, lost, 40, 4)), "entry 2");
        assert!(!viewport.plain_plan(retained, 102, 40, 4).anchor_evicted);
    }

    #[test]
    fn tp4_appending_while_reading_never_moves_the_anchor() {
        let mut content = entries(6);
        let mut viewport = TranscriptViewport::new();
        assert!(!viewport.is_reading());
        viewport.scroll_rows(-2);
        assert!(viewport.is_reading());
        let before = viewport.plain_plan(&content, 0, 40, 4);
        content.extend(entries(3));
        let after = viewport.plain_plan(&content, 0, 40, 4);
        assert_eq!(before, after);
        viewport.scroll_rows(i32::MAX);
        assert!(viewport.is_reading());
        let bottom = viewport.plain_plan(&content, 0, 40, 4);
        assert!(bottom.reading);
        content.push("another entry".into());
        assert_eq!(viewport.plain_plan(&content, 0, 40, 4), bottom);
        viewport.follow_latest();
        assert!(!viewport.is_reading());
        let following = viewport.plain_plan(&content, 0, 40, 4);
        assert!(!following.reading);
        assert!(following.start_entry > bottom.start_entry);
    }

    #[test]
    fn tp1_resize_clamps_row_inside_same_entry() {
        let content = vec!["word ".repeat(50), "last".into()];
        let mut viewport = TranscriptViewport::new();
        viewport.scroll_rows(-2);
        let narrow = viewport.plain_plan(&content, 10, 26, 3);
        assert_eq!(narrow.start_entry, 0);
        assert!(narrow.row_offset > 0);
        let wide = viewport.plain_plan(&content, 10, 200, 3);
        assert_eq!(wide.start_entry, 0);
        assert_eq!((wide.line_offset, wide.row_offset), (1, 0));
        assert!(wide.reading);
        assert!(!wide.anchor_evicted);
    }

    #[test]
    fn tp1_large_global_history_and_single_entry_avoid_u16_scroll_limit() {
        let content: Vec<String> = (0..200)
            .map(|index| format!("{}end {index}", "x\n".repeat(350)))
            .collect();
        let mut viewport = TranscriptViewport::new();
        let end = viewport.plain_plan(&content, 0, 40, 3);
        assert_eq!(end.start_entry, 199);
        assert_eq!(end.line_offset, 349);
        assert_eq!(first_line(&render(&content, end, 40, 3)), "x");
        let single = vec![format!("{}tail", "\n".repeat(70_000))];
        let last = viewport.plain_plan(&single, 500, 26, 3);
        assert_eq!(last.line_offset, 69_999);
        let buffer = render(&single, last, 26, 3);
        assert_eq!(buffer[(0, 1)].symbol(), "t");
        assert_eq!(last.row_offset, 0);
    }

    #[test]
    fn tp1_tiny_geometry_preserves_anchor_and_queued_navigation() {
        let content = entries(10);
        let mut viewport = TranscriptViewport::new();
        viewport.scroll_rows(-2);
        let before = viewport.plain_plan(&content, 0, 40, 4);
        viewport.scroll_rows(-2);
        for (width, height) in [(0, 0), (25, 3), (26, 2)] {
            let empty = viewport.plain_plan(&content, 0, width, height);
            assert_eq!(empty.start_entry, content.len());
            assert!(empty.reading);
        }
        let after = viewport.plain_plan(&content, 0, 40, 4);
        assert_eq!(after.start_entry, before.start_entry - 1);
        assert_eq!(viewport.plain_plan(&[], 100, 40, 4).start_entry, 0);
    }

    #[test]
    fn tp1_plan_matches_paragraph_wrapping_including_unicode_and_blank_lines() {
        let content = vec![
            "界 e\u{301} 👨‍👩‍👧‍👦 words keep wrapping across the visible line\nnext line".into(),
            "\nlast".into(),
        ];
        let mut viewport = TranscriptViewport::new();
        let plan = viewport.plain_plan(&content, 0, 26, 3);
        let all_lines: Vec<Line<'_>> = content
            .iter()
            .flat_map(|entry| entry.lines().chain(std::iter::once("")))
            .map(Line::from)
            .collect();
        let paragraph = Paragraph::new(all_lines).wrap(Wrap { trim: false });
        let offset = u16::try_from(paragraph.line_count(26).saturating_sub(3)).unwrap();
        let area = Rect::new(0, 0, 26, 3);
        let mut expected = Buffer::empty(area);
        paragraph.scroll((offset, 0)).render(area, &mut expected);
        assert_eq!(render(&content, plan, 26, 3), expected);
    }

    #[test]
    fn ts3_decorated_layout_anchors_content_edges_and_separator() {
        let layouts = [
            EntryLayout::new("first\nsecond", 26, true, false),
            EntryLayout::new("following", 26, false, true),
        ];
        let mut viewport = TranscriptViewport::new();
        viewport.scroll_rows(-100);
        for expected in 0..4 {
            let plan = viewport.plan(&layouts, 42, 3);
            assert_eq!(
                (plan.start_entry, plan.line_offset, plan.row_offset),
                (0, expected, 0)
            );
            viewport.scroll_rows(1);
        }
        assert_eq!(
            layouts[0]
                .lines
                .iter()
                .map(|line| line.kind)
                .collect::<Vec<_>>(),
            [
                LineKind::Upper,
                LineKind::Text,
                LineKind::Text,
                LineKind::Lower,
            ]
        );
        // A reading anchor survives a larger viewport even below its last full page.
        let resized = viewport.plan(&layouts, 42, 20);
        assert!(resized.reading);
        assert_eq!(resized.line_offset, 3);
        let compact = [EntryLayout::new("first\nsecond", 26, false, true)];
        let clamped = viewport.plan(&compact, 42, 3);
        assert_eq!(clamped.line_offset, 2);
        assert_eq!(compact[0].lines[2].kind, LineKind::Separator);
        let empty = EntryLayout::new("", 26, false, false);
        assert_eq!(empty.lines.len(), 1);
        assert_eq!(empty.lines[0].rows, 1);
    }
}
