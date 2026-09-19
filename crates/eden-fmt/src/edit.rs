//! Byte-offset bookkeeping for the two rewriters.
//!
//! `proc-macro2` reports a position as a 1-based line and a 0-based **character**
//! column within that line: its spans index characters, and `offset_line_column`
//! subtracts character offsets. A line-start table plus the text itself converts
//! any span into the byte range of the exact source that produced it.

use proc_macro2::{LineColumn, Span};
use std::ops::Range;

/// Start offset of every line of a text, plus the text itself.
pub struct LineIndex<'a> {
    text: &'a str,
    starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    pub fn new(text: &'a str) -> Self {
        let mut starts = vec![0];
        for (offset, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                starts.push(offset + 1);
            }
        }
        Self { text, starts }
    }

    /// Byte offset of a span position, or `None` when it does not describe this text.
    pub fn offset(&self, at: LineColumn) -> Option<usize> {
        let line = at.line.checked_sub(1)?;
        let start = *self.starts.get(line)?;
        let end = self
            .starts
            .get(line + 1)
            .map_or(self.text.len(), |next| next - 1);
        let rest = self.text.get(start..end)?;
        if at.column > rest.chars().count() {
            return None;
        }
        Some(
            start
                + rest
                    .char_indices()
                    .nth(at.column)
                    .map_or(rest.len(), |(at, _)| at),
        )
    }

    /// Byte range of a span, or `None` when either end is unrepresentable.
    pub fn range(&self, span: Span) -> Option<Range<usize>> {
        let start = self.offset(span.start())?;
        let end = self.offset(span.end())?;
        (start <= end).then_some(start..end)
    }

    /// 1-based line number containing a byte offset.
    pub fn line(&self, offset: usize) -> usize {
        match self.starts.binary_search(&offset) {
            Ok(index) => index + 1,
            Err(index) => index,
        }
    }

    /// 0-based character column of a byte offset within its line.
    pub fn column(&self, offset: usize) -> usize {
        let start = self.starts[self.line(offset) - 1];
        self.text
            .get(start..offset)
            .map_or(0, |text| text.chars().count())
    }
}

/// Column reached after appending `text` at `column`.
///
/// The width counts characters rather than bytes: the fit rule compares a
/// rendered line against `max_width` the way rustfmt does, not the way the
/// allocator does.
pub fn advance_column(column: usize, text: &str) -> usize {
    match text.rfind('\n') {
        Some(index) => text[index + 1..].chars().count(),
        None => column + text.chars().count(),
    }
}

/// True when a source slice carries a comment, which no layout decision may drop.
pub fn has_comment(text: &str) -> bool {
    text.contains("//") || text.contains("/*")
}
