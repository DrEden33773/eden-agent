use crate::model::{Anchor, Message, Preferences, Role, Row, TextRole, TextSpan, ToolState};
#[path = "rich_text.rs"]
mod rich_text;
use rich_text::{RichLine, code_lines, markdown};
#[path = "tool_text.rs"]
mod tool_text;
use similar::{ChangeTag, TextDiff};
use tool_text::{tool_output, tool_title};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub fn clean(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if c == '\t' {
            output.push_str("    ");
        } else if c == '\n' || !c.is_control() {
            output.push(c);
        }
    }
    output
}
pub fn clipped(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for g in text.graphemes(true) {
        if used + g.width() > width {
            break;
        }
        out.push_str(g);
        used += g.width();
    }
    out
}
pub fn cell_byte(text: &str, column: usize) -> usize {
    let mut width = 0;
    for (byte, g) in text.grapheme_indices(true) {
        if width + g.width() > column {
            return byte;
        }
        width += g.width();
    }
    text.len()
}
pub fn word_span(text: &str, column: usize) -> (usize, usize) {
    let byte = cell_byte(text, column);
    let start = text[..byte]
        .char_indices()
        .rfind(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let end = text[byte..]
        .find(char::is_whitespace)
        .map_or(text.len(), |i| byte + i);
    (start, end)
}
pub fn wrap(text: &str, width: usize) -> Vec<(usize, String)> {
    let width = width.max(1);
    let mut out = vec![];
    let mut line = String::new();
    let mut used = 0;
    let mut start = 0;
    for (byte, g) in text.grapheme_indices(true) {
        let w = g.width();
        if used + w > width && !line.is_empty() {
            out.push((start, std::mem::take(&mut line)));
            start = byte;
            used = 0;
        }
        if w > width {
            line.push('�');
            used += 1;
        } else {
            line.push_str(g);
            used += w;
        }
    }
    out.push((start, line));
    out
}
fn diff_rows(
    before: &str,
    after: &str,
    width: usize,
    split: bool,
    language: &str,
) -> Vec<RichLine> {
    let diff = TextDiff::from_lines(before, after);
    let old = code_lines(before, language);
    let new = code_lines(after, language);
    let changes: Vec<_> = diff.iter_all_changes().collect();
    let added = changes
        .iter()
        .filter(|c| c.tag() == ChangeTag::Insert)
        .count();
    let removed = changes
        .iter()
        .filter(|c| c.tag() == ChangeTag::Delete)
        .count();
    let mut stats = RichLine::default();
    stats.push(&format!("+{added}"), &[TextRole::DiffAdded]);
    stats.push(" ", &[]);
    stats.push(&format!("−{removed}"), &[TextRole::DiffRemoved]);
    let bars = width.saturating_sub(stats.text.width() + 3).min(12);
    if bars > 0 && added + removed > 0 {
        let green = (bars * added).div_ceil(added + removed);
        stats.push("  ", &[]);
        stats.push(&"━".repeat(green), &[TextRole::DiffAdded]);
        stats.push(&"━".repeat(bars - green), &[TextRole::DiffRemoved]);
    }
    let mut out = vec![stats];
    let cell = |line: Option<usize>, tag: ChangeTag, source: &[RichLine]| {
        let mut cell = RichLine::default();
        if let Some(index) = line {
            let role = match tag {
                ChangeTag::Insert => TextRole::DiffAdded,
                ChangeTag::Delete => TextRole::DiffRemoved,
                ChangeTag::Equal => TextRole::DiffGutter,
            };
            cell.push(
                &format!(
                    "{} {:>3} ",
                    match tag {
                        ChangeTag::Insert => "+",
                        ChangeTag::Delete => "−",
                        ChangeTag::Equal => " ",
                    },
                    index + 1
                ),
                &[role],
            );
            if let Some(body) = source.get(index) {
                let start = cell.text.len();
                if tag != ChangeTag::Equal {
                    cell.spans.push(TextSpan {
                        range: start..start + body.text.len(),
                        role,
                    });
                }
                cell.append(body);
            }
        }
        cell
    };
    let half = width.saturating_sub(3) / 2;
    let gutter = old.len().max(new.len()).to_string().len().max(3) + 3;
    let fits = old
        .iter()
        .chain(new.iter())
        .all(|line| line.text.width() + gutter <= half);
    if split && width >= 120 && fits {
        let half = width.saturating_sub(3) / 2;
        let mut header = RichLine::default();
        header.push(
            &format!("{:<half$} │ {}", "BEFORE", "AFTER"),
            &[TextRole::DiffGutter],
        );
        out.push(header);
        let mut left: Vec<RichLine> = vec![];
        let mut right: Vec<RichLine> = vec![];
        let flush =
            |out: &mut Vec<RichLine>, left: &mut Vec<RichLine>, right: &mut Vec<RichLine>| {
                for i in 0..left.len().max(right.len()) {
                    let mut line = left
                        .get(i)
                        .map_or_else(RichLine::default, |s| s.clipped(half));
                    let pad = half.saturating_sub(line.text.width());
                    line.push(&format!("{} │ ", " ".repeat(pad)), &[TextRole::DiffGutter]);
                    if let Some(right) = right.get(i) {
                        line.append(&right.clipped(width.saturating_sub(half + 3)));
                    }
                    out.push(line);
                }
                left.clear();
                right.clear();
            };
        for change in changes {
            match change.tag() {
                ChangeTag::Delete => left.push(cell(change.old_index(), ChangeTag::Delete, &old)),
                ChangeTag::Insert => right.push(cell(change.new_index(), ChangeTag::Insert, &new)),
                ChangeTag::Equal => {
                    flush(&mut out, &mut left, &mut right);
                    left.push(cell(change.old_index(), ChangeTag::Equal, &old));
                    right.push(cell(change.new_index(), ChangeTag::Equal, &new));
                    flush(&mut out, &mut left, &mut right);
                }
            }
        }
        flush(&mut out, &mut left, &mut right);
    } else {
        for change in changes {
            out.push(match change.tag() {
                ChangeTag::Delete => cell(change.old_index(), ChangeTag::Delete, &old),
                tag => cell(change.new_index(), tag, &new),
            });
        }
    }
    out
}
#[cfg(test)]
pub fn diff_lines(before: &str, after: &str, width: usize, split: bool) -> Vec<String> {
    diff_rows(before, after, width, split, "text")
        .into_iter()
        .map(|line| line.text)
        .collect()
}
fn message_language(m: &Message) -> &str {
    m.title
        .split_whitespace()
        .find_map(|part| part.rsplit_once('.').map(|(_, extension)| extension))
        .unwrap_or("text")
}
pub fn message_rows(m: &Message, width: usize, p: &Preferences) -> Vec<Row> {
    render_message(m, width, p, false)
}
#[cfg(test)]
thread_local! { pub static INSPECTOR_LAYOUTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

pub fn inspector_rows(m: &Message, width: usize, p: &Preferences) -> Vec<Row> {
    #[cfg(test)]
    INSPECTOR_LAYOUTS.with(|count| count.set(count.get() + 1));
    render_message(m, width, p, true)
}
fn render_message(m: &Message, width: usize, p: &Preferences, full: bool) -> Vec<Row> {
    let expanded = m.expanded || full;
    if let Some(summary) = &m.summary {
        let (text, numbers) = summary.formatted();
        if text.is_empty() {
            return vec![];
        }
        let mut rich = RichLine::plain(text);
        rich.mark(TextRole::Muted);
        rich.mark(TextRole::Emphasis);
        for (range, failed) in numbers {
            rich.spans.push(TextSpan {
                range,
                role: if failed {
                    TextRole::DiffRemoved
                } else {
                    TextRole::DiffAdded
                },
            });
        }
        // The rail is painted outside the logical text, so soft wrap never changes copied content.
        rich.inset = 2;
        return wrap_lines(m, width, vec![rich, RichLine::default()]);
    }
    let pending = if m.children.is_empty() {
        m.pending
    } else {
        m.children
            .iter()
            .any(|child| child.state == ToolState::Pending)
    };
    let failed = if m.children.is_empty() {
        m.failed
    } else {
        m.children
            .iter()
            .any(|child| child.state == ToolState::Failed)
    };
    let marker = match m.role {
        Role::User => "›",
        Role::Assistant => "",
        Role::Thinking => "◌",
        Role::Tool if pending => "⠿",
        Role::Tool if failed => "✗",
        Role::Tool => "✓",
        Role::Notice => "!",
    };
    let speaker_title =
        if m.role == Role::Assistant && m.title != "Eden" && !m.title.starts_with("Eden · ") {
            format!("Eden · {}", m.title)
        } else {
            m.title.clone()
        };
    let mut title = RichLine::plain(format!("{marker} {}", clean(&speaker_title)).trim_start());
    title.mark(TextRole::Strong);
    if m.role == Role::Tool {
        title = tool_title(&format!("{marker} "), &m.title, failed, pending);
        title.hanging = 2;
    }
    if !m.children.is_empty() {
        let successful = m
            .children
            .iter()
            .filter(|child| child.state == ToolState::Success)
            .count();
        let pending = m
            .children
            .iter()
            .filter(|child| child.state == ToolState::Pending)
            .count();
        let failed = m
            .children
            .iter()
            .filter(|child| child.state == ToolState::Failed)
            .count();
        title.push(&format!("  {} tools", m.children.len()), &[TextRole::Muted]);
        for (count, marker, role) in [
            (successful, "✓", TextRole::DiffAdded),
            (pending, "◌", TextRole::InlineCode),
            (failed, "✗", TextRole::DiffRemoved),
        ] {
            if count > 0 {
                title.push(&format!("  {marker} {count}"), &[role]);
            }
        }
    }
    let mut lines = vec![title];
    match m.role {
        _ if !m.children.is_empty() => {
            for (index, child) in m.children.iter().enumerate() {
                let last = index + 1 == m.children.len();
                let marker = match child.state {
                    ToolState::Pending => "◌",
                    ToolState::Success => "✓",
                    ToolState::Failed => "✗",
                };
                let prefix = format!("  {} {marker} ", if last { "└─" } else { "├─" });
                let mut line = tool_title(
                    &prefix,
                    &child.title,
                    child.state == ToolState::Failed,
                    child.state == ToolState::Pending,
                );
                line.hanging = 7;
                line.tree_stem = !last;
                // Collapsed rows summarize; expanded rows move the result into the body once.
                if !expanded && !full {
                    line.push(" · ", &[TextRole::Muted]);
                    let brief = if child.state == ToolState::Pending {
                        "Running…"
                    } else {
                        child.detail.lines().next().unwrap_or("")
                    };
                    for detail in tool_output(&child.title, brief, width.saturating_sub(7)) {
                        line.append(&detail);
                    }
                }
                lines.push(line);
                if expanded || full {
                    let body = if child.state == ToolState::Pending {
                        "Waiting for tool output…"
                    } else {
                        &child.detail
                    };
                    let details = tool_output(&child.title, body, width.saturating_sub(7));
                    let limit = if full { details.len() } else { 4 };
                    let omitted = details.len().saturating_sub(limit);
                    for mut detail in details.into_iter().take(limit) {
                        detail.inset = 7;
                        detail.tree_stem = !last;
                        if child.state == ToolState::Failed {
                            detail.mark(TextRole::DiffRemoved);
                        }
                        lines.push(detail);
                    }
                    if omitted > 0 {
                        let mut hint = RichLine::plain(format!(
                            "… {omitted} more lines · Inspect full output"
                        ));
                        hint.dim = true;
                        hint.inset = 7;
                        hint.tree_stem = !last;
                        lines.push(hint);
                    }
                }
            }
            if !expanded && !full {
                let mut hint = RichLine::plain("↳ click to show more");
                hint.inset = 2;
                hint.dim = true;
                lines.push(hint);
            }
        }
        _ if p.basic => lines.extend(m.body.lines().map(|s| RichLine::plain(clean(s)))),
        Role::Tool if !expanded => {
            if !p.compact || m.before.is_some() || m.failed || m.pending {
                let summary = if m.pending {
                    "Running…".to_owned()
                } else if let Some(preview) = &m.preview {
                    preview.clone()
                } else if m.before.is_some() {
                    m.body.lines().next().unwrap_or("").to_owned()
                } else if m.failed {
                    m.body.clone()
                } else {
                    m.body.lines().last().unwrap_or("Result ready").to_owned()
                };
                let summary = format!("{summary} · click to show more");
                for line in clean(&summary).lines() {
                    let mut summary = RichLine::plain(format!("↳ {line}"));
                    summary.inset = 2;
                    summary.hanging = 2;
                    summary.dim = true;
                    lines.push(summary);
                }
            }
            if let (Some(before), Some(after)) = (&m.before, &m.after) {
                for mut line in diff_rows(
                    before,
                    after,
                    width.saturating_sub(2),
                    p.diff_split,
                    message_language(m),
                )
                .into_iter()
                .take(if p.compact { 4 } else { 9 })
                {
                    line.inset = 2;
                    lines.push(line);
                }
            }
        }
        Role::Thinking if !p.thinking => {
            let mut line = RichLine::plain("  Hidden · Ctrl+T to show");
            line.dim = true;
            lines.push(line);
        }
        Role::Thinking if !expanded => {
            for mut line in markdown(
                &m.body,
                width.saturating_sub(if m.role == Role::Tool { 2 } else { 0 }),
            )
            .into_iter()
            .take(3)
            {
                line.prefix("  ", TextRole::Muted);
                line.dim = true;
                lines.push(line);
            }
        }
        _ => {
            if let (Some(before), Some(after)) = (&m.before, &m.after) {
                lines.extend(diff_rows(
                    before,
                    after,
                    width.saturating_sub(2),
                    p.diff_split,
                    message_language(m),
                ));
            } else if m.role == Role::Tool {
                let mut content = tool_output(&m.title, &m.body, width.saturating_sub(2));
                if !full && content.len() > 14 {
                    let omitted = content.len() - 14;
                    content.truncate(14);
                    let mut hint =
                        RichLine::plain(format!("… {omitted} more lines · Inspect full output"));
                    hint.dim = true;
                    content.push(hint);
                }
                lines.extend(content);
            } else {
                lines.extend(markdown(
                    &m.body,
                    width.saturating_sub(if m.role == Role::Tool { 2 } else { 0 }),
                ));
            }
        }
    }
    if m.role == Role::Tool && m.children.is_empty() {
        for line in lines.iter_mut().skip(1) {
            line.inset = 2;
        }
    }
    if expanded && matches!(m.role, Role::Tool | Role::Thinking) && !full {
        let mut hint = RichLine::plain("↑ Collapse");
        hint.inset = 2;
        hint.dim = true;
        lines.push(hint);
    }
    lines.push(RichLine::default());
    wrap_lines(m, width, lines)
}

fn wrap_hanging(text: &str, width: usize, hanging: usize) -> Vec<(usize, String)> {
    let mut rows = vec![];
    let mut start = 0;
    while start < text.len() {
        let available = width
            .saturating_sub(if rows.is_empty() { 0 } else { hanging })
            .max(1);
        let mut used = 0;
        let mut end = start;
        let mut word_break = None;
        for (offset, grapheme) in text[start..].grapheme_indices(true) {
            if used + grapheme.width() > available {
                break;
            }
            used += grapheme.width();
            end = start + offset + grapheme.len();
            if grapheme.chars().all(char::is_whitespace) {
                word_break = Some(end);
            }
        }
        if end == start {
            let next = text[start..].graphemes(true).next().unwrap();
            rows.push((start, "�".into()));
            start += next.len();
            continue;
        }
        if end < text.len()
            && let Some(boundary) = word_break
        {
            end = boundary;
        }
        rows.push((start, text[start..end].into()));
        start = end;
    }
    if rows.is_empty() {
        rows.push((0, String::new()));
    }
    rows
}

fn wrap_lines(m: &Message, width: usize, lines: Vec<RichLine>) -> Vec<Row> {
    let mut rows = vec![];
    for (line, rich) in lines.into_iter().enumerate() {
        let inset = rich.inset.min(width.saturating_sub(1));
        let hanging = rich.hanging.min(width.saturating_sub(inset + 1));
        let wrapped = wrap_hanging(&rich.text, width.saturating_sub(inset), hanging);
        for (row_index, (byte, text)) in wrapped.iter().enumerate() {
            let byte = *byte;
            let end = wrapped
                .get(row_index + 1)
                .map_or(rich.text.len(), |(offset, _)| *offset);
            let mapping: Vec<_> = rich.text[byte..end]
                .grapheme_indices(true)
                .zip(text.grapheme_indices(true))
                .collect();
            let spans = rich
                .spans
                .iter()
                .filter_map(|span| {
                    let mut mapped = mapping
                        .iter()
                        .filter(|((source, grapheme), _)| {
                            byte + source + grapheme.len() > span.range.start
                                && byte + source < span.range.end
                        })
                        .map(|(_, (offset, grapheme))| *offset..offset + grapheme.len());
                    let first = mapped.next()?;
                    let stop = mapped.next_back().map_or(first.end, |last| last.end);
                    Some(TextSpan {
                        range: first.start..stop,
                        role: span.role,
                    })
                })
                .collect();
            let row_inset = inset + if row_index == 0 { 0 } else { hanging };
            rows.push(Row {
                image: None,
                tree_stem: rich.tree_stem && row_inset > 2,
                inset: row_inset as u16,
                summary: m.summary.is_some(),
                anchor: Anchor {
                    record: m.id,
                    line,
                    byte,
                },
                text: text.clone(),
                source: (rich.text[byte..end] != *text).then(|| rich.text[byte..end].to_owned()),
                spans,
                kind: m.role,
                dim: rich.dim,
                failed: m.failed,
            });
        }
    }
    rows
}
pub fn locate(rows: &[Row], anchor: Anchor) -> usize {
    rows.iter()
        .position(|r| r.anchor == anchor)
        .or_else(|| {
            rows.iter()
                .rposition(|r| r.anchor.record == anchor.record && r.anchor <= anchor)
        })
        .or_else(|| rows.iter().position(|r| r.anchor.record == anchor.record))
        .unwrap_or(0)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn group_expansion_replaces_inline_preview_with_detail_and_pending_has_no_future_result() {
        let mut message = Message::new(82, Role::Tool, "Multiple tools", "");
        message.children = vec![crate::model::ToolSummary {
            title: "Read sample.rs".into(),
            detail: "1 line loaded\n1  let answer = 42;".into(),
            state: ToolState::Success,
        }];
        let preferences = Preferences::default();
        let collapsed = message_rows(&message, 80, &preferences);
        assert!(
            collapsed
                .iter()
                .any(|row| row.text.contains("Read sample.rs · 1 line loaded"))
        );
        assert!(!collapsed.iter().any(|row| row.text.contains("answer")));
        message.expanded = true;
        let expanded = message_rows(&message, 80, &preferences);
        assert_eq!(
            expanded
                .iter()
                .filter(|row| row.text.contains("1 line loaded"))
                .count(),
            1
        );
        assert!(expanded.iter().any(|row| row.text.contains("answer")));
        assert!(
            expanded
                .iter()
                .any(|row| row.text.ends_with("Read sample.rs"))
        );
        message.children[0].state = ToolState::Pending;
        let pending = message_rows(&message, 80, &preferences);
        assert!(
            pending
                .iter()
                .any(|row| row.text == "Waiting for tool output…")
        );
        assert!(
            !pending
                .iter()
                .any(|row| row.text.contains("answer") || row.text.contains("1 line loaded"))
        );
    }

    #[test]
    fn tool_hanging_wrap_keeps_names_and_copy_boundaries() {
        let mut message = Message::new(
            81,
            Role::Tool,
            "Read src/long_中文_filename.rs",
            "48  let 中文 = something_long;\n49  next_line();",
        );
        message.expanded = true;
        for width in [12, 24, 80] {
            let rows = message_rows(&message, width, &Preferences::default());
            assert!(
                rows.iter()
                    .all(|row| row.text.width() + row.inset as usize <= width)
            );
            let body = rows.iter().find(|row| row.anchor.line == 1).unwrap();
            assert_eq!(body.inset, 2);
            assert_eq!(
                crate::selection::Selection::line(&rows, body.anchor)
                    .unwrap()
                    .text(&rows),
                "48  let 中文 = something_long;"
            );
            assert!(
                rows.iter()
                    .filter(|row| row.anchor.line == 0 && row.anchor.byte > 0)
                    .all(|row| row.inset == 2)
            );
        }
    }

    #[test]
    fn summary_wrap_preserves_copy_and_number_styles_without_a_heading() {
        let message = Message::run_summary(
            10,
            crate::summary::RunSummary {
                commands: 2,
                reads: 3,
                failed: 1,
                duration_ms: 42_000,
                ..crate::summary::RunSummary::default()
            },
        );
        let wide = message_rows(&message, 120, &Preferences::default());
        assert_eq!(wide[0].text, "Ran 2 commands, read 3 files, 1 failed · 42s");
        let selection = crate::selection::Selection::line(&wide, wide[0].anchor).unwrap();
        for width in [3, 4, 8, 20, 80] {
            let rows = message_rows(&message, width, &Preferences::default());
            assert_eq!(selection.text(&rows), message.body, "width {width}");
            assert!(
                rows.iter()
                    .all(|row| row.summary && row.text.width() <= width - 2)
            );
            assert!(
                rows.iter()
                    .filter(|row| !row.text.is_empty())
                    .all(|row| row.spans.iter().any(|span| span.role == TextRole::Emphasis))
            );
        }
        let numbers: Vec<_> = wide[0]
            .spans
            .iter()
            .filter(|span| matches!(span.role, TextRole::DiffAdded | TextRole::DiffRemoved))
            .map(|span| (&wide[0].text[span.range.clone()], span.role))
            .collect();
        assert_eq!(
            numbers,
            vec![
                ("2", TextRole::DiffAdded),
                ("3", TextRole::DiffAdded),
                ("1", TextRole::DiffRemoved)
            ]
        );
    }
    #[test]
    fn tool_group_counts_come_from_members_and_expansion_preserves_full_details() {
        let mut message = Message::new(9, Role::Tool, "Multiple Tools", "obsolete summary");
        message.children = vec![
            crate::model::ToolSummary {
                title: "Read src/中文.rs".into(),
                detail: "ready".into(),
                state: ToolState::Success,
            },
            crate::model::ToolSummary {
                title: "Bash test".into(),
                detail: "waiting".into(),
                state: ToolState::Pending,
            },
            crate::model::ToolSummary {
                title: "Read missing".into(),
                detail: "not found\n2\n3\n4\n5\nlast detail".into(),
                state: ToolState::Failed,
            },
        ];
        let preferences = Preferences::default();
        let compact = message_rows(&message, 80, &preferences);
        assert_eq!(compact[0].text, "⠿ Multiple Tools  3 tools  ✓ 1  ◌ 1  ✗ 1");
        assert!(
            compact
                .iter()
                .any(|r| r.text.contains("├─ ✓ Read src/中文.rs"))
        );
        assert!(compact.iter().any(|r| r.text.contains("not found")));
        assert!(!compact.iter().any(|r| r.text.contains("obsolete")));
        message.expanded = true;
        let expanded = message_rows(&message, 80, &preferences);
        assert!(expanded.iter().any(|r| r.text.contains("not found")));
        assert!(
            expanded
                .iter()
                .any(|r| r.text.contains("Inspect full output"))
        );
        let full = inspector_rows(&message, 80, &preferences);
        assert!(full.iter().any(|r| r.text.contains("last detail")));
        assert!(!full.iter().any(|r| r.text.contains("Inspect full output")));
    }
    #[test]
    fn clean_removes_complete_terminal_sequences() {
        assert_eq!(
            clean(
                "\u{1b}[31merror\u{1b}[0m \u{1b}]8;;https://example.com\u{1b}\\link\u{1b}]8;;\u{1b}\\"
            ),
            "error link"
        );
    }
    #[test]
    fn styled_unicode_wrap_keeps_ranges_valid_and_copy_plain() {
        let message = Message::new(3, Role::Assistant, "Eden", "**中文e\u{301}🙂** and `value`");
        for width in [1, 2, 5, 20] {
            let rows = message_rows(&message, width, &Preferences::default());
            assert!(rows.iter().all(|row| {
                row.spans.iter().all(|span| {
                    span.range.end <= row.text.len()
                        && row.text.is_char_boundary(span.range.start)
                        && row.text.is_char_boundary(span.range.end)
                })
            }));
            assert!(rows.iter().all(|row| !row.text.contains('\u{1b}')));
        }
        assert_eq!(message.body, "**中文e\u{301}🙂** and `value`");
    }
    #[test]
    fn diff_fallback_measures_expanded_tabs_before_selecting_split() {
        let content = format!("{}TAIL_MARKER\n", "\t".repeat(20));
        let rows = diff_rows("old\n", &content, 120, true, "rust");
        assert!(
            rows.iter()
                .map(|r| r.text.as_str())
                .collect::<String>()
                .contains("TAIL_MARKER")
        );
    }
    #[test]
    fn narrow_diff_preview_keeps_long_line_tail() {
        let content = format!("{}TAIL_MARKER\n", "long_content_".repeat(20));
        let mut m = Message::new(1, Role::Tool, "Edit sample.rs", "");
        m.before = Some("old\n".into());
        m.after = Some(content);
        m.expanded = true;
        let rows = inspector_rows(&m, 80, &Preferences::default());
        assert!(
            rows.iter()
                .map(|r| r.text.as_str())
                .collect::<String>()
                .contains("TAIL_MARKER")
        );
    }
    #[test]
    fn split_diff_colors_both_sides_independently() {
        let rows = diff_rows(
            "let old = \"中文\";\n",
            "let new = 42;\n",
            140,
            true,
            "rust",
        );
        let changed = rows
            .iter()
            .find(|line| line.text.contains("old") && line.text.contains("new"))
            .unwrap();
        assert!(
            changed
                .spans
                .iter()
                .any(|span| span.role == TextRole::DiffRemoved
                    && changed.text[span.range.clone()].contains("old"))
        );
        assert!(
            changed
                .spans
                .iter()
                .any(|span| span.role == TextRole::DiffAdded
                    && changed.text[span.range.clone()].contains("new"))
        );
        assert!(
            changed
                .spans
                .iter()
                .any(|span| span.role == TextRole::CodeKeyword)
        );
        assert!(
            changed
                .spans
                .iter()
                .any(|span| span.role == TextRole::CodeNumber)
        );
    }
    #[test]
    fn wrapping_preserves_graphemes() {
        let s = "中文e\u{301}🙂👨‍👩‍👧‍👦abc";
        let rows = wrap(s, 6);
        assert_eq!(rows.iter().map(|(_, s)| s.as_str()).collect::<String>(), s);
        assert!(rows.iter().all(|(_, s)| s.width() <= 6));
    }
    #[test]
    fn anchor_survives_rewrap() {
        let mut m = Message::new(
            7,
            Role::Assistant,
            "Eden",
            "alpha beta gamma delta\n中文再次确认",
        );
        m.expanded = true;
        let wide = message_rows(&m, 18, &Preferences::default());
        let a = wide[2].anchor;
        let narrow = message_rows(&m, 8, &Preferences::default());
        assert_eq!(narrow[locate(&narrow, a)].anchor.record, 7);
        assert_eq!(narrow[locate(&narrow, a)].anchor.line, a.line);
    }
    #[test]
    fn diff_contains_real_changes() {
        let lines = diff_lines("a\nb\n", "a\nc\n", 40, false);
        assert!(lines.iter().any(|s| s.starts_with('−') && s.ends_with('b')));
        assert!(lines.iter().any(|s| s.starts_with('+') && s.ends_with('c')));
    }
    #[test]
    fn word_selection_after_ideographic_space_is_utf8_safe() {
        let text = "a\u{3000}b";
        let (start, end) = word_span(text, 3);
        assert_eq!(&text[start..end], "b");
    }
}
