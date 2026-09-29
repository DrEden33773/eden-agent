use crate::{
    model::{Anchor, Row},
    text,
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Hit {
    before: Anchor,
    after: Anchor,
}
impl Hit {
    fn at(row: &Row, column: usize) -> Self {
        let source = source(row);
        let mut cell = 0;
        for ((byte, original), displayed) in
            source.grapheme_indices(true).zip(row.text.graphemes(true))
        {
            let end = cell + displayed.width();
            if column < end {
                let before = Anchor {
                    byte: row.anchor.byte + byte,
                    ..row.anchor
                };
                let after = Anchor {
                    byte: before.byte + if column > cell { original.len() } else { 0 },
                    ..before
                };
                return Self { before, after };
            }
            cell = end;
        }
        Self::boundary(Anchor {
            byte: row.anchor.byte + source.len(),
            ..row.anchor
        })
    }
    fn boundary(anchor: Anchor) -> Self {
        Self {
            before: anchor,
            after: anchor,
        }
    }
}
fn source(row: &Row) -> &str {
    row.source.as_deref().unwrap_or(&row.text)
}

fn grapheme_boundary(text: &str, byte: usize) -> bool {
    byte == text.len() || text.grapheme_indices(true).any(|(start, _)| start == byte)
}
fn boundary_prefix(rows: &[Row], anchor: Anchor) -> Option<String> {
    let mut prefix = String::new();
    for row in rows
        .iter()
        .filter(|row| (row.anchor.record, row.anchor.line) == (anchor.record, anchor.line))
    {
        let content = source(row);
        let end = row.anchor.byte + content.len();
        if anchor.byte >= row.anchor.byte && anchor.byte <= end {
            let byte = anchor.byte - row.anchor.byte;
            if !grapheme_boundary(content, byte) {
                return None;
            }
            prefix.push_str(content.get(..byte)?);
            return Some(prefix);
        }
        prefix.push_str(content);
    }
    None
}

/// Captured only when selected records are rebuilt, never on each animation frame.
pub struct Checkpoint {
    selection: Selection,
    start_prefix: Option<String>,
    end_prefix: Option<String>,
    selected: String,
}
impl Checkpoint {
    pub fn matches(&self, rows: &[Row]) -> bool {
        let (start, end) = self.selection.bounds();
        self.start_prefix.is_some()
            && self.end_prefix.is_some()
            && boundary_prefix(rows, start) == self.start_prefix
            && boundary_prefix(rows, end) == self.end_prefix
            && self.selection.text(rows) == self.selected
    }
}

/// Endpoints refer to logical lines and grapheme byte boundaries, independent of wrapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    origin: Hit,
    focus: Hit,
}
impl Selection {
    pub fn from_row(row: &Row, column: usize) -> Self {
        let hit = Hit::at(row, column);
        Self {
            origin: hit,
            focus: hit,
        }
    }
    pub fn extend(&mut self, row: &Row, column: usize) {
        self.focus = Hit::at(row, column);
    }
    pub fn is_empty(&self) -> bool {
        self.origin == self.focus
    }
    pub fn origin(&self) -> Anchor {
        self.origin.before
    }
    fn bounds(&self) -> (Anchor, Anchor) {
        if self.is_empty() {
            return (self.origin.before, self.origin.before);
        }
        if self.origin <= self.focus {
            (self.origin.before, self.focus.after)
        } else {
            (self.focus.before, self.origin.after)
        }
    }
    pub fn contains_record(&self, record: u64) -> bool {
        let (start, end) = self.bounds();
        (start.record..=end.record).contains(&record)
    }
    pub fn checkpoint(&self, rows: &[Row]) -> Checkpoint {
        let (start, end) = self.bounds();
        Checkpoint {
            selection: *self,
            start_prefix: boundary_prefix(rows, start),
            end_prefix: boundary_prefix(rows, end),
            selected: self.text(rows),
        }
    }
    pub fn line(rows: &[Row], anchor: Anchor) -> Option<Self> {
        let mut matching = rows
            .iter()
            .filter(|r| (r.anchor.record, r.anchor.line) == (anchor.record, anchor.line));
        let first = matching.next()?;
        let last = matching.next_back().unwrap_or(first);
        let mut selection = Self::from_row(first, 0);
        selection.extend(last, last.text.width());
        Some(selection)
    }
    pub fn word(rows: &[Row], anchor: Anchor) -> Self {
        let line: String = rows
            .iter()
            .filter(|r| (r.anchor.record, r.anchor.line) == (anchor.record, anchor.line))
            .map(source)
            .collect();
        let byte = line
            .grapheme_indices(true)
            .map(|(byte, _)| byte)
            .chain(std::iter::once(line.len()))
            .take_while(|&byte| byte <= anchor.byte)
            .last()
            .unwrap_or(0);
        let (start, end) = text::word_span(&line, line[..byte].width());
        Self {
            origin: Hit::boundary(Anchor {
                byte: start,
                ..anchor
            }),
            focus: Hit::boundary(Anchor {
                byte: end,
                ..anchor
            }),
        }
    }
    fn row_bytes(&self, row: &Row) -> Range<usize> {
        let (start, end) = self.bounds();
        let row_start = row.anchor;
        let row_end = Anchor {
            byte: row_start.byte + source(row).len(),
            ..row_start
        };
        if self.is_empty() || end <= row_start || start >= row_end {
            return 0..0;
        }
        let from = if (start.record, start.line) == (row_start.record, row_start.line) {
            start
                .byte
                .saturating_sub(row_start.byte)
                .min(source(row).len())
        } else {
            0
        };
        let to = if (end.record, end.line) == (row_start.record, row_start.line) {
            end.byte
                .saturating_sub(row_start.byte)
                .min(source(row).len())
        } else {
            source(row).len()
        };
        if !grapheme_boundary(source(row), from) || !grapheme_boundary(source(row), to) {
            return 0..0;
        }
        from..to
    }
    /// Both copy and paint use the same source byte range; oversized fallback glyphs retain their source.
    pub fn row_cells(&self, row: &Row) -> Range<usize> {
        let bytes = self.row_bytes(row);
        if bytes.is_empty() {
            return 0..0;
        }
        let mut column = 0;
        let mut start = None;
        let mut end = 0;
        for ((byte, original), displayed) in source(row)
            .grapheme_indices(true)
            .zip(row.text.graphemes(true))
        {
            let next = column + displayed.width();
            if byte < bytes.end && byte + original.len() > bytes.start {
                start.get_or_insert(column);
                end = next;
            }
            column = next;
        }
        start.unwrap_or(0)..end
    }
    pub fn text(&self, rows: &[Row]) -> String {
        if self.is_empty() {
            return String::new();
        }
        let (start, end) = self.bounds();
        if boundary_prefix(rows, start).is_none() || boundary_prefix(rows, end).is_none() {
            return String::new();
        }
        let first = text::locate(rows, start);
        let last = text::locate(rows, end);
        let mut output = String::new();
        let mut previous = None;
        for row in rows.iter().take(last + 1).skip(first) {
            let key = (row.anchor.record, row.anchor.line);
            if previous.is_some_and(|old| old != key) {
                output.push('\n');
            }
            let bytes = self.row_bytes(row);
            let Some(selected) = source(row).get(bytes) else {
                return String::new();
            };
            output.push_str(selected);
            previous = Some(key);
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Message, Preferences, Role};
    fn rows(body: &str, width: usize) -> Vec<Row> {
        text::message_rows(
            &Message::new(1, Role::Assistant, "Eden", body),
            width,
            &Preferences::default(),
        )
    }
    #[test]
    fn streaming_checkpoint_invalidates_changed_prefix_or_selected_content() {
        let before = rows("**中文", 40);
        let mut selection = Selection::from_row(&before[1], 2);
        selection.extend(&before[1], 4);
        let checkpoint = selection.checkpoint(&before);
        assert!(!checkpoint.matches(&rows("**中文**", 40)));
        assert!(!checkpoint.matches(&rows("##中文", 40)));
        assert!(!checkpoint.matches(&rows("**英文", 40)));
    }
    #[test]
    fn streaming_checkpoint_preserves_suffix_appends_and_all_wrap_widths() {
        let before = rows("prefix 中文 tail", 40);
        let mut selection = Selection::from_row(&before[1], 7);
        selection.extend(&before[1], 11);
        let checkpoint = selection.checkpoint(&before);
        for width in 1..=30 {
            let after = rows("prefix 中文 tail appended\nnext line", width);
            assert!(checkpoint.matches(&after), "width {width}");
            assert_eq!(selection.text(&after), "中文");
        }
    }
    #[test]
    fn checkpoint_detects_grapheme_merge_at_selection_end() {
        let before = rows("e", 40);
        let mut selection = Selection::from_row(&before[1], 0);
        selection.extend(&before[1], 1);
        let after = rows("e\u{301}", 40);
        assert!(!selection.checkpoint(&before).matches(&after));
        assert_eq!(selection.text(&after), "");
    }
    #[test]
    fn completed_streaming_markdown_never_slices_stale_utf8_offsets() {
        let before = rows("**中文", 40);
        let mut selection = Selection::from_row(&before[1], 2);
        selection.extend(&before[1], 4);
        assert_eq!(selection.text(&before), "中");
        let completed = rows("**中文**", 40);
        assert_eq!(selection.text(&completed), "");
    }
    #[test]
    fn partial_wide_graphemes_snap_outward_in_both_directions() {
        let rows = rows("中文e\u{301}👩‍💻X", 40);
        let row = &rows[1];
        for (start, end, expected, columns) in [
            (0, 3, "中文", 0..4),
            (3, 0, "中文", 0..4),
            (4, 6, "e\u{301}👩‍💻", 4..7),
            (6, 4, "e\u{301}👩‍💻", 4..7),
        ] {
            let mut selection = Selection::from_row(row, start);
            selection.extend(row, end);
            assert_eq!(selection.text(&rows), expected);
            assert_eq!(selection.row_cells(row), columns);
        }
    }
    #[test]
    fn selection_keeps_content_through_every_wrap_width_including_replacement_glyphs() {
        let body = "中文e\u{301}👩‍💻 tail\n第二行";
        let original = rows(body, 40);
        let mut selection = Selection::from_row(&original[1], 0);
        selection.extend(&original[2], original[2].text.width());
        for width in 1..=30 {
            assert_eq!(selection.text(&rows(body, width)), body, "width={width}");
        }
    }
    #[test]
    fn copying_soft_wrap_never_adds_a_newline_but_empty_logical_line_remains() {
        let body = "甲乙丙丁戊己庚辛壬癸\n\n末行";
        let rows = rows(body, 4);
        let first = rows.iter().find(|r| r.anchor.line == 1).unwrap();
        let last = rows.iter().rfind(|r| !r.text.is_empty()).unwrap();
        let mut selection = Selection::from_row(first, 0);
        selection.extend(last, last.text.width());
        assert_eq!(selection.text(&rows), body);
    }
    #[test]
    fn word_and_line_selection_include_all_soft_wrapped_segments() {
        let rows = rows("prefix 中文连续字符 suffix", 6);
        let anchor = Anchor {
            record: 1,
            line: 1,
            byte: "prefix 中".len(),
        };
        assert_eq!(Selection::word(&rows, anchor).text(&rows), "中文连续字符");
        assert_eq!(
            Selection::line(&rows, anchor).unwrap().text(&rows),
            "prefix 中文连续字符 suffix"
        );
    }
}
