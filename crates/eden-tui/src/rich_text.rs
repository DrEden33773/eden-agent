#[path = "diagram.rs"]
mod diagram;
#[path = "math.rs"]
mod math;
use crate::model::{TextRole, TextSpan};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::sync::OnceLock;
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, Default)]
pub(super) struct RichLine {
    pub text: String,
    pub spans: Vec<TextSpan>,
    pub dim: bool,
    pub inset: usize,
    pub hanging: usize,
    pub tree_stem: bool,
}
impl RichLine {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }
    pub fn push(&mut self, text: &str, roles: &[TextRole]) {
        let start = self.text.len();
        self.text.push_str(text);
        for &role in roles {
            if start < self.text.len() {
                self.spans.push(TextSpan {
                    range: start..self.text.len(),
                    role,
                });
            }
        }
    }
    pub fn prefix(&mut self, prefix: &str, role: TextRole) {
        for span in &mut self.spans {
            span.range.start += prefix.len();
            span.range.end += prefix.len();
        }
        self.text.insert_str(0, prefix);
        self.spans.insert(
            0,
            TextSpan {
                range: 0..prefix.len(),
                role,
            },
        );
    }
    pub fn mark(&mut self, role: TextRole) {
        if !self.text.is_empty() {
            self.spans.push(TextSpan {
                range: 0..self.text.len(),
                role,
            });
        }
    }
    pub fn append(&mut self, other: &Self) {
        let offset = self.text.len();
        self.text.push_str(&other.text);
        self.spans.extend(other.spans.iter().map(|span| TextSpan {
            range: span.range.start + offset..span.range.end + offset,
            role: span.role,
        }));
    }
    fn slice(&self, start: usize, end: usize) -> Self {
        Self {
            text: self.text[start..end].to_owned(),
            spans: self
                .spans
                .iter()
                .filter_map(|span| {
                    let begin = start.max(span.range.start);
                    let stop = end.min(span.range.end);
                    (begin < stop).then(|| TextSpan {
                        range: begin - start..stop - start,
                        role: span.role,
                    })
                })
                .collect(),
            dim: self.dim,
            inset: self.inset,
            hanging: self.hanging,
            tree_stem: self.tree_stem,
        }
    }
    pub fn clipped(&self, width: usize) -> Self {
        let text = super::clipped(&self.text, width);
        let end = text.len();
        let spans = self
            .spans
            .iter()
            .filter_map(|span| {
                let stop = span.range.end.min(end);
                (span.range.start < stop).then_some(TextSpan {
                    range: span.range.start..stop,
                    role: span.role,
                })
            })
            .collect();
        Self {
            text,
            spans,
            dim: self.dim,
            inset: self.inset,
            hanging: self.hanging,
            tree_stem: self.tree_stem,
        }
    }
}

const MAX_SYNTAX_CHARS: usize = 32_000;

fn within_syntax_budget(source: &str) -> bool {
    source.len() <= MAX_SYNTAX_CHARS
        || source.chars().take(MAX_SYNTAX_CHARS + 1).count() <= MAX_SYNTAX_CHARS
}

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

pub(super) struct CodeHighlighter {
    parser: ParseState,
    scopes: ScopeStack,
}
impl CodeHighlighter {
    pub fn new(language: &str) -> Self {
        let language = match language {
            "ts" | "typescript" | "tsx" => "js",
            "rs" => "rust",
            "sh" | "shell" | "console" => "bash",
            other => other,
        };
        let set = syntaxes();
        let syntax = set
            .find_syntax_by_token(language)
            .unwrap_or_else(|| set.find_syntax_plain_text());
        Self {
            parser: ParseState::new(syntax),
            scopes: ScopeStack::new(),
        }
    }
    pub fn line(&mut self, text: &str) -> RichLine {
        // Newline-aware grammars need the terminator even though terminal rows omit it.
        let input = format!("{text}\n");
        let mut result = RichLine::plain(text);
        if let Ok(operations) = self.parser.parse_line(&input, syntaxes()) {
            let mut start = 0;
            for (end, operation) in operations {
                self.add_span(&mut result, start, end.min(text.len()));
                let _ = self.scopes.apply(&operation);
                start = end.min(text.len());
            }
            self.add_span(&mut result, start, text.len());
        }
        result
    }
    fn add_span(&self, line: &mut RichLine, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let role = self.scopes.as_slice().iter().rev().find_map(|scope| {
            let scope = scope.to_string();
            if scope.starts_with("comment") {
                Some(TextRole::CodeComment)
            } else if scope.starts_with("string") {
                Some(TextRole::CodeString)
            } else if scope.starts_with("constant.numeric") {
                Some(TextRole::CodeNumber)
            } else if scope.starts_with("keyword")
                || scope.starts_with("storage")
                || scope.starts_with("constant.language")
            {
                Some(TextRole::CodeKeyword)
            } else if scope.starts_with("entity.name.function")
                || scope.starts_with("support.function")
            {
                Some(TextRole::CodeFunction)
            } else if scope.starts_with("entity.name.type")
                || scope.starts_with("support.type")
                || scope.starts_with("support.class")
            {
                Some(TextRole::CodeType)
            } else {
                None
            }
        });
        if let Some(role) = role {
            line.spans.push(TextSpan {
                range: start..end,
                role,
            });
        }
    }
}

pub(super) fn markdown(source: &str, width: usize) -> Vec<RichLine> {
    let source = super::clean(source);
    let raw_lines: Vec<&str> = source.lines().collect();
    if raw_lines.is_empty() {
        return vec![];
    }
    let mut starts = vec![0];
    starts.extend(source.match_indices('\n').map(|(i, _)| i + 1));
    let line_at = |byte: usize| {
        starts
            .partition_point(|&start| start <= byte)
            .saturating_sub(1)
            .min(raw_lines.len() - 1)
    };
    let mut lines = vec![RichLine::default(); raw_lines.len()];
    let mut roles = Vec::new();
    let mut level_one = vec![];
    let mut quote_depth = 0;
    let mut code: Option<CodeHighlighter> = None;
    let mut in_code = false;
    let mut fenced = false;
    let mut special_blocks = vec![];
    let mut replacements = vec![];
    let mut table_start = 0;
    let mut table_depth = 0;
    let mut in_table = false;
    let mut tables = vec![];
    let mut list_depth: usize = 0;
    let mut table_column = 0;
    let mut links: Vec<String> = vec![];
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH;
    for (event, range) in Parser::new_ext(&source, options).into_offset_iter() {
        let index = line_at(range.start);
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    let depth = level as usize;
                    if depth == 1 {
                        level_one.push(index);
                    }
                    if depth >= 3 {
                        lines[index].push(&format!("{} ", "#".repeat(depth)), &[TextRole::Heading]);
                    }
                    roles.push(TextRole::Heading);
                }
                Tag::List(_) => list_depth += 1,
                Tag::Strong => roles.push(TextRole::Strong),
                Tag::Emphasis => roles.push(TextRole::Emphasis),
                Tag::Strikethrough => roles.push(TextRole::Strike),
                Tag::BlockQuote(_) => {
                    quote_depth += 1;
                    roles.push(TextRole::Quote);
                }
                Tag::Link { dest_url, .. } => {
                    roles.push(TextRole::Link);
                    links.push(dest_url.into_string());
                }
                Tag::Image { .. } => lines[index].push("[image] ", &[TextRole::Muted]),
                Tag::Item => {
                    let trimmed = raw_lines[index]
                        .trim_start()
                        .trim_start_matches('>')
                        .trim_start();
                    let marker = trimmed.split_whitespace().next().unwrap_or("-");
                    let bullet = if marker.ends_with('.') || marker.ends_with(')') {
                        marker
                    } else {
                        "-"
                    };
                    lines[index].inset = 4 * list_depth.saturating_sub(1);
                    lines[index].push(&format!("{bullet} "), &[TextRole::ListMarker]);
                    lines[index].hanging = lines[index].text.width();
                }
                Tag::CodeBlock(kind) => {
                    let language = match &kind {
                        CodeBlockKind::Fenced(info) => {
                            info.split_whitespace().next().unwrap_or("text")
                        }
                        _ => "text",
                    };
                    if matches!(language, "mermaid" | "latex" | "tex" | "math") {
                        special_blocks.push((
                            index,
                            line_at(range.end.saturating_sub(1)),
                            language.to_owned(),
                        ));
                    }
                    fenced = matches!(kind, CodeBlockKind::Fenced(_));
                    if fenced {
                        lines[index].push(&format!("╭─ {language}"), &[TextRole::CodeFence]);
                    }
                    in_code = true;
                    code = within_syntax_budget(&source[range])
                        .then(|| CodeHighlighter::new(language));
                }
                Tag::Table(_) => {
                    table_start = index;
                    table_depth = quote_depth;
                    in_table = true;
                }
                Tag::TableHead => {
                    table_column = 0;
                    roles.push(TextRole::Strong);
                }
                Tag::TableRow => table_column = 0,
                Tag::TableCell => {
                    if table_column > 0 {
                        lines[index].push(" │ ", &[TextRole::Muted]);
                    }
                    table_column += 1;
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::List(_) => list_depth = list_depth.saturating_sub(1),
                TagEnd::Heading(_) | TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
                    roles.pop();
                }
                TagEnd::BlockQuote(_) => {
                    quote_depth -= 1;
                    roles.pop();
                }
                TagEnd::Link => {
                    roles.pop();
                    if let Some(url) = links.pop() {
                        // Keep destinations visible and copyable without emitting terminal OSC links.
                        if !lines[index].text.ends_with(&url) {
                            lines[index].push(&format!(" ({url})"), &[TextRole::Link]);
                        }
                    }
                }
                TagEnd::CodeBlock => {
                    if fenced {
                        let end = line_at(range.end.saturating_sub(1));
                        if raw_lines[end].trim_start().starts_with("```")
                            || raw_lines[end].trim_start().starts_with("~~~")
                        {
                            lines[end].push("╰─", &[TextRole::CodeFence]);
                        }
                    }
                    code = None;
                    in_code = false;
                }
                TagEnd::TableHead => {
                    roles.pop();
                }
                TagEnd::Table => {
                    tables.push((
                        table_start,
                        line_at(range.end.saturating_sub(1)),
                        table_depth,
                    ));
                    in_table = false;
                }
                _ => {}
            },
            Event::Text(text) => {
                for (offset, part) in text.split_terminator('\n').enumerate() {
                    let target = (index + offset).min(lines.len() - 1);
                    if in_code {
                        let mut line = code.as_mut().map_or_else(
                            || RichLine::plain(part),
                            |highlighter| highlighter.line(part),
                        );
                        line.prefix("│ ", TextRole::CodeFence);
                        lines[target].append(&line);
                    } else {
                        if quote_depth > 0 && !in_table && lines[target].text.is_empty() {
                            lines[target].push(&"│ ".repeat(quote_depth), &[TextRole::Quote]);
                        }
                        lines[target].push(part, &roles);
                    }
                }
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                let rendered =
                    math::render(&text).unwrap_or_else(|| source[range.clone()].to_owned());
                for (offset, part) in rendered.lines().enumerate() {
                    lines[(index + offset).min(raw_lines.len() - 1)].push(part, &roles);
                }
            }
            Event::Code(text) => {
                if quote_depth > 0 && !in_table && lines[index].text.is_empty() {
                    lines[index].push(&"│ ".repeat(quote_depth), &[TextRole::Quote]);
                }
                let mut inline = roles.clone();
                inline.push(TextRole::InlineCode);
                lines[index].push(&text, &inline);
            }
            Event::TaskListMarker(checked) => {
                lines[index].push(
                    if checked { "[x] " } else { "[ ] " },
                    &[TextRole::ListMarker],
                );
                lines[index].hanging = lines[index].text.width();
            }
            Event::Rule => lines[index].push(&"─".repeat(width.min(80)), &[TextRole::Muted]),
            Event::Html(text) | Event::InlineHtml(text) => {
                for (offset, part) in text.lines().enumerate() {
                    lines[(index + offset).min(raw_lines.len() - 1)].push(part, &[TextRole::Muted]);
                }
            }
            _ => {}
        }
    }
    // Layout after parsing: a table's visual rows must not shift the source indices
    // used by later parser events. Reverse replacement preserves earlier ranges.
    for index in level_one {
        lines[index].mark(TextRole::Underline);
    }
    for (start, end, language) in special_blocks {
        let opening = raw_lines[start].trim_start();
        let fence = opening.chars().next().unwrap_or('`');
        let count = opening.chars().take_while(|&ch| ch == fence).count();
        let closing = raw_lines[end].trim();
        let closed = end > start && closing.chars().all(|ch| ch == fence) && closing.len() >= count;
        let body = if closed {
            raw_lines[start + 1..end].join("\n")
        } else {
            String::new()
        };
        let rendered = if !closed || !within_syntax_budget(&body) {
            None
        } else if language == "mermaid" {
            diagram::render(&body, width)
        } else {
            math::render(&body).map(|text| text.lines().map(str::to_owned).collect())
        };
        let rendered = rendered.unwrap_or_else(|| {
            raw_lines[start..=end]
                .iter()
                .map(|line| (*line).to_owned())
                .collect()
        });
        replacements.push((
            start,
            end,
            rendered
                .into_iter()
                .map(RichLine::plain)
                .collect::<Vec<_>>(),
        ));
    }
    for (start, end, depth) in tables {
        let rendered = render_table(&lines[start..=end], &raw_lines[start..=end], width, depth);
        replacements.push((start, end, rendered));
    }
    replacements.sort_by_key(|(start, _, _)| *start);
    for (start, end, rendered) in replacements.into_iter().rev() {
        lines.splice(start..=end, rendered);
    }
    lines
}

fn table_cells(lines: &[RichLine]) -> Vec<Vec<RichLine>> {
    lines
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 1)
        .map(|(_, line)| {
            let mut start = 0;
            let mut cells = vec![];
            for separator in line.spans.iter().filter(|span| {
                span.role == TextRole::Muted && &line.text[span.range.clone()] == " │ "
            }) {
                cells.push(line.slice(start, separator.range.start));
                start = separator.range.end;
            }
            cells.push(line.slice(start, line.text.len()));
            cells
        })
        .collect()
}

fn column_widths(rows: &[Vec<RichLine>], width: usize) -> Option<Vec<usize>> {
    let columns = rows.first()?.len();
    let available = width.checked_sub(3 * columns + 1)?;
    if columns == 0 || available < columns {
        return None;
    }
    let mut natural = vec![0; columns];
    let mut minimum = vec![1; columns];
    for row in rows {
        for (index, cell) in row.iter().enumerate().take(columns) {
            natural[index] = natural[index].max(cell.text.width());
            let longest = cell
                .text
                .split_whitespace()
                .map(UnicodeWidthStr::width)
                .max()
                .unwrap_or(0)
                .min(30);
            minimum[index] = minimum[index].max(longest);
        }
    }
    if minimum.iter().sum::<usize>() > available {
        let weights: Vec<_> = minimum.iter().map(|size| size - 1).collect();
        let total: usize = weights.iter().sum();
        let remaining = available - columns;
        minimum = weights
            .iter()
            .map(|weight| 1 + (weight * remaining).checked_div(total).unwrap_or(0))
            .collect();
        let leftover = available - minimum.iter().sum::<usize>();
        for size in minimum.iter_mut().take(leftover) {
            *size += 1;
        }
    }
    if natural.iter().sum::<usize>() <= available {
        return Some(
            natural
                .into_iter()
                .zip(minimum)
                .map(|(natural, min)| natural.max(min))
                .collect(),
        );
    }
    let extra = available.saturating_sub(minimum.iter().sum());
    let weights: Vec<_> = natural
        .iter()
        .zip(&minimum)
        .map(|(natural, min)| natural.saturating_sub(*min))
        .collect();
    let total: usize = weights.iter().sum();
    let mut widths: Vec<_> = minimum
        .into_iter()
        .zip(weights)
        .map(|(min, weight)| min + (weight * extra).checked_div(total).unwrap_or(0))
        .collect();
    let mut remaining = available - widths.iter().sum::<usize>();
    while remaining > 0 {
        let before = remaining;
        for (width, natural) in widths.iter_mut().zip(&natural) {
            if remaining > 0 && *width < *natural {
                *width += 1;
                remaining -= 1;
            }
        }
        if remaining == before {
            break;
        }
    }
    Some(widths)
}

fn wrap_cell(cell: &RichLine, width: usize) -> Vec<RichLine> {
    if cell.text.is_empty() {
        return vec![cell.clone()];
    }
    let mut result = vec![];
    let mut start = 0;
    while start < cell.text.len() {
        let remaining = &cell.text[start..];
        let mut used = 0;
        let mut end = start;
        let mut whitespace = None;
        for (offset, grapheme) in remaining.grapheme_indices(true) {
            if used + grapheme.width() > width {
                break;
            }
            used += grapheme.width();
            end = start + offset + grapheme.len();
            if grapheme.chars().all(char::is_whitespace) && offset > 0 {
                whitespace = Some(start + offset);
            }
        }
        if end < cell.text.len()
            && !cell.text[end..].starts_with(char::is_whitespace)
            && let Some(boundary) = whitespace
        {
            end = boundary;
        }
        // render_table rejects columns narrower than their widest grapheme.
        if end == start {
            return vec![cell.clone()];
        }
        let trimmed = cell.text[start..end].trim_end().len();
        result.push(cell.slice(start, start + trimmed));
        start = end;
        while start < cell.text.len() {
            let Some(ch) = cell.text[start..].chars().next() else {
                break;
            };
            if !ch.is_whitespace() {
                break;
            }
            start += ch.len_utf8();
        }
    }
    result
}

fn table_border(widths: &[usize], left: &str, middle: &str, right: &str) -> RichLine {
    let mut line = RichLine::default();
    line.push(left, &[TextRole::Muted]);
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            line.push(middle, &[TextRole::Muted]);
        }
        line.push(&"─".repeat(*width + 2), &[TextRole::Muted]);
    }
    line.push(right, &[TextRole::Muted]);
    line
}

fn render_table(
    source: &[RichLine],
    raw: &[&str],
    width: usize,
    quote_depth: usize,
) -> Vec<RichLine> {
    let rows = table_cells(source);
    let available = width.saturating_sub(2 * quote_depth);
    let fallback = || raw.iter().map(|line| RichLine::plain(*line)).collect();
    let Some(widths) = column_widths(&rows, available) else {
        return fallback();
    };
    // A 2-cell glyph cannot fit a 1-cell column. Preserve it in the same raw
    // fallback used for impossible border budgets instead of replacing content.
    if rows.iter().any(|row| {
        row.iter().zip(&widths).any(|(cell, width)| {
            cell.text
                .graphemes(true)
                .any(|grapheme| grapheme.width() > *width)
        })
    }) {
        return fallback();
    }
    let mut output = vec![table_border(&widths, "┌", "┬", "┐")];
    for (row_index, cells) in rows.iter().enumerate() {
        let wrapped: Vec<_> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| wrap_cell(cell, *width))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for part in 0..height {
            let mut line = RichLine::default();
            line.push("│", &[TextRole::Muted]);
            for (column, width) in widths.iter().enumerate() {
                line.push(" ", &[]);
                let begin = line.text.len();
                let cell = wrapped.get(column).and_then(|parts| parts.get(part));
                if let Some(cell) = cell {
                    line.append(cell);
                }
                let padding = width.saturating_sub(cell.map_or(0, |cell| cell.text.width()));
                line.push(&" ".repeat(padding), &[]);
                if row_index == 0 && begin < line.text.len() {
                    line.spans.push(TextSpan {
                        range: begin..line.text.len(),
                        role: TextRole::Strong,
                    });
                }
                line.push(" ", &[]);
                line.push("│", &[TextRole::Muted]);
            }
            output.push(line);
        }
        if row_index + 1 < rows.len() {
            output.push(table_border(&widths, "├", "┼", "┤"));
        }
    }
    output.push(table_border(&widths, "└", "┴", "┘"));
    if quote_depth > 0 {
        let prefix = "│ ".repeat(quote_depth);
        for line in &mut output {
            line.prefix(&prefix, TextRole::Quote);
        }
    }
    output
}

pub(super) fn code_lines(source: &str, language: &str) -> Vec<RichLine> {
    let mut highlighter = within_syntax_budget(source).then(|| CodeHighlighter::new(language));
    source
        .lines()
        .map(|line| {
            let clean = super::clean(line);
            highlighter.as_mut().map_or_else(
                || RichLine::plain(&clean),
                |highlighter| highlighter.line(&clean),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn render(source: &str) -> Vec<RichLine> {
        markdown(source, 100)
    }
    fn fragments(line: &RichLine, role: TextRole) -> String {
        line.spans
            .iter()
            .filter(|span| span.role == role)
            .map(|span| &line.text[span.range.clone()])
            .collect()
    }
    fn joined(source: &str, width: usize) -> String {
        markdown(source, width)
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn mermaid_flow_renders_nodes_edges_and_preserves_following_markdown() {
        let output = joined(
            "```mermaid\nflowchart LR\nA[Start] --> B[Done]\n```\n\n**After**",
            80,
        );
        assert!(output.contains("[Start] ──▶ [Done]"), "{output}");
        assert!(output.ends_with("After"));
        assert!(!output.contains("flowchart LR"));
    }
    #[test]
    fn mermaid_sequence_resolves_aliases_and_message_direction() {
        let output = joined(
            "```mermaid\nsequenceDiagram\nparticipant A as Alice\nparticipant B as Bob\nA->>B: \
             Hello\nB-->>A: Reply\n```",
            80,
        );
        assert!(output.contains("Alice ──▶ Bob: Hello"), "{output}");
        assert!(output.contains("Bob ╌╌▶ Alice: Reply"), "{output}");
    }
    #[test]
    fn rich_syntax_falls_back_without_losing_unknown_or_partial_source() {
        for source in [
            "```mermaid\nflowchart LR\nA --> B",
            "```mermaid\npie\n\"A\": 2\n```",
            "```latex\n\\unknown{x}\n```",
            "```mermaid\nflowchart LR\nA --> B\nclick A call()\n```",
        ] {
            assert_eq!(joined(source, 80), source);
        }
    }
    #[test]
    fn latex_renders_inline_and_fenced_math_without_consuming_code() {
        let output = joined(
            r"Energy $E = mc^2$ and $\alpha_1 + \frac{a}{b}$; `$x^2$`.",
            80,
        );
        assert_eq!(output, "Energy E = mc² and α₁ + (a)/(b); $x^2$.");
        assert!(joined("```latex\n\\sqrt{x^2 + y^2}\n```", 80).contains("√(x² + y²)"));
    }
    #[test]
    fn diagram_narrow_layout_keeps_labels_and_arrows() {
        let output = joined("```mermaid\nflowchart LR\nA[中文开始] --> B[完成]\n```", 12);
        assert!(output.contains("[中文开始]\n  │\n  ▼\n[完成]"), "{output}");
        assert!(output.lines().all(|line| line.width() <= 12));
    }
    #[test]
    fn mixed_table_diagram_and_math_replacements_keep_order_and_styles() {
        let source = "| A |\n| - |\n| **table** |\n\n```mermaid\nflowchart LR\nA --> \
                      B\n```\n\n**Math $x^2$ end**\n\n| B |\n| - |\n| final |";
        let lines = markdown(source, 40);
        let all = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.find("table").unwrap() < all.find("[A]").unwrap());
        assert!(all.find("[B]").unwrap() < all.find("Math x² end").unwrap());
        assert!(all.find("Math x² end").unwrap() < all.find("final").unwrap());
        assert!(
            lines
                .iter()
                .any(|line| fragments(line, TextRole::Strong) == "Math x² end")
        );
    }
    #[test]
    fn incomplete_and_unknown_math_stays_visible() {
        for source in [
            r"$x^2",
            r"$\unknown{x}$",
            r"$x^\alpha$",
            r"$x^}$",
            r"$\frac{a}{$",
            "$$\nx^2",
            "```latex\nx^2",
        ] {
            assert_eq!(joined(source, 80), source);
        }
        assert!(joined("$$\nx^2 + y^2\n$$", 80).contains("x² + y²"));
    }
    #[test]
    fn diagrams_and_formula_fallback_never_emit_terminal_escape() {
        for source in [
            "```mermaid\nflowchart LR\nA[\x1b[31mRed] --> B\n```",
            "```latex\n\\unknown{\x1b]52;c;payload\x07x}\n```",
        ] {
            let output = joined(source, 80);
            assert!(!output.contains('\x1b'));
            assert!(!output.contains("payload"));
        }
    }
    #[test]
    fn table_preserves_empty_leading_middle_and_trailing_cells() {
        let lines = render(
            "| Left | Middle | Right |\n| --- | --- | --- |\n| | center | last |\n| first | | \
             last |\n| first | center | |\n| | | |",
        );
        let cells: Vec<Vec<_>> = lines
            .iter()
            .filter(|line| line.text.starts_with('│'))
            .map(|line| {
                line.text
                    .trim_matches('│')
                    .split('│')
                    .map(str::trim)
                    .collect()
            })
            .collect();
        assert_eq!(
            cells,
            vec![
                vec!["Left", "Middle", "Right"],
                vec!["", "center", "last"],
                vec!["first", "", "last"],
                vec!["first", "center", ""],
                vec!["", "", ""],
            ]
        );
        let widths: Vec<_> = lines.iter().map(|line| line.text.width()).collect();
        assert!(widths.iter().all(|width| *width == widths[0]));
    }
    #[test]
    fn tables_have_complete_borders_and_row_separators() {
        let lines = markdown("| A | B |\n| - | - |\n| one | two |\n| x | y |", 80);
        let rendered: Vec<_> = lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(
            rendered,
            vec![
                "┌─────┬─────┐",
                "│ A   │ B   │",
                "├─────┼─────┤",
                "│ one │ two │",
                "├─────┼─────┤",
                "│ x   │ y   │",
                "└─────┴─────┘",
            ]
        );
    }
    #[test]
    fn table_wraps_cells_without_breaking_borders_or_english_words() {
        let lines = markdown(
            "| Name | Detail |\n| --- | --- |\n| 中文 | alpha beta gamma delta epsilon |",
            26,
        );
        assert!(lines.iter().all(|line| line.text.width() <= 26));
        let body: Vec<_> = lines
            .iter()
            .filter(|line| line.text.starts_with('│'))
            .collect();
        assert!(body.len() > 2);
        assert!(body.iter().all(|line| line.text.matches('│').count() == 3));
        let words: Vec<_> = body
            .iter()
            .skip(1)
            .flat_map(|line| line.text.split('│').nth(2).unwrap().split_whitespace())
            .collect();
        assert_eq!(words, vec!["alpha", "beta", "gamma", "delta", "epsilon"]);
        assert!(body.iter().any(|line| line.text.contains("中文")));
    }
    #[test]
    fn table_long_inline_code_retains_styles_without_styling_borders() {
        let token = "identifier_中文_value_with_a_long_suffix";
        let lines = markdown(&format!("| Code |\n| --- |\n| `{token}` |"), 18);
        assert!(lines.iter().all(|line| line.text.width() <= 18));
        let fragments: String = lines
            .iter()
            .flat_map(|line| {
                line.spans
                    .iter()
                    .filter(|span| span.role == TextRole::InlineCode)
                    .map(|span| &line.text[span.range.clone()])
            })
            .collect();
        assert_eq!(fragments, token);
        assert!(lines.iter().all(|line| {
            line.spans
                .iter()
                .filter(|span| span.role != TextRole::Muted)
                .all(|span| !line.text[span.range.clone()].contains('│'))
        }));
    }
    #[test]
    fn table_narrow_fallback_preserves_original_markdown() {
        let source = "| Left | Right |\n| --- | --- |\n| 中文 | value |";
        let lines = markdown(source, 8);
        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            source
        );
    }
    #[test]
    fn table_one_cell_column_falls_back_instead_of_losing_wide_graphemes() {
        let source = "| A | B |\n| - | - |\n| 中 | 文 |";
        let lines = markdown(source, 9);
        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            source
        );
    }
    #[test]
    fn table_replacements_keep_following_blocks_in_order() {
        let lines = markdown(
            "| First |\n| --- |\n| long first value |\n\nAfter first.\n\n| Second |\n| --- |\n| \
             last |\n\nAfter second.",
            16,
        );
        let joined = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.find("After first.").unwrap() < joined.find("Second").unwrap());
        assert!(joined.find("Second").unwrap() < joined.find("After second.").unwrap());
    }
    #[test]
    fn lists_set_hanging_layout_without_inserting_wrapped_copy_text() {
        let lines = markdown(
            "- parent item with long text\n  - child item\n- [x] checked",
            12,
        );
        assert_eq!(lines[0].text, "- parent item with long text");
        assert_eq!((lines[0].inset, lines[0].hanging), (0, 2));
        assert_eq!((lines[1].inset, lines[1].hanging), (4, 2));
        assert_eq!((lines[2].inset, lines[2].hanging), (0, 6));
        let clipped = lines[1].clipped(6);
        assert_eq!((clipped.inset, clipped.hanging), (4, 2));
    }
    #[test]
    fn heading_levels_and_rules_respect_reference_structure_and_width() {
        let lines = markdown("# One\n## Two\n### Three\n\n---", 13);
        assert_eq!(lines[0].text, "One");
        assert_eq!(lines[1].text, "Two");
        assert_eq!(lines[2].text, "### Three");
        assert_eq!(lines[4].text, "─────────────");
    }
    #[test]
    fn oversized_fenced_code_keeps_all_content_without_grammar_work() {
        let source = format!("{}fn final_line() {{}}", "let n = 42;\n".repeat(3_000));
        let lines = render(&format!("```rust\n{source}\n```"));
        let body = &lines[1..lines.len() - 1];
        assert_eq!(
            body.iter()
                .map(|line| line.text.strip_prefix("│ ").unwrap())
                .collect::<Vec<_>>()
                .join("\n"),
            source
        );
        assert!(body.iter().all(|line| {
            line.spans
                .iter()
                .all(|span| span.role == TextRole::CodeFence)
        }));
    }
    #[test]
    fn oversized_tool_code_keeps_all_content_without_grammar_work() {
        let source = format!("{}let final_line = 42;", "let n = 42;\n".repeat(3_000));
        let lines = code_lines(&source, "rust");
        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            source
        );
        assert!(lines.iter().all(|line| line.spans.is_empty()));
    }
    #[test]
    fn syntax_budget_counts_unicode_characters() {
        assert!(within_syntax_budget(&"中".repeat(MAX_SYNTAX_CHARS)));
        assert!(!within_syntax_budget(&"中".repeat(MAX_SYNTAX_CHARS + 1)));
    }
    #[test]
    fn markdown_renders_semantics_without_delimiters() {
        let lines = render(
            "# Summary\n\n**Bold** and *quiet* with `run()` and [docs](https://example.com).\n> A \
             quotation\n- [x] tested\n\n| Name | State |\n| --- | --- |\n| Test | Passed |",
        );
        assert_eq!(fragments(&lines[0], TextRole::Heading), "Summary");
        assert_eq!(fragments(&lines[2], TextRole::Strong), "Bold");
        assert_eq!(fragments(&lines[2], TextRole::Emphasis), "quiet");
        assert_eq!(fragments(&lines[2], TextRole::InlineCode), "run()");
        assert!(lines[2].text.contains("docs (https://example.com)"));
        assert_eq!(lines[3].text, "│ A quotation");
        assert_eq!(lines[4].text, "- [x] tested");
        assert!(lines.iter().any(|line| line.text.contains("Name │ State")));
        assert!(!lines.iter().any(|line| line.text.contains("**")));
    }
    #[test]
    fn fenced_code_uses_grammar_and_tracks_multiline_comments() {
        let lines = render(
            "```rust\nfn greet() {\n    /* start\n       end */ let n = 42;\n    \
             println!(\"hello\");\n}\n```",
        );
        assert_eq!(lines[0].text, "╭─ rust");
        assert!(fragments(&lines[1], TextRole::CodeKeyword).contains("fn"));
        assert!(fragments(&lines[1], TextRole::CodeFunction).contains("greet"));
        assert!(fragments(&lines[3], TextRole::CodeComment).contains("end"));
        assert!(fragments(&lines[3], TextRole::CodeNumber).contains("42"));
        assert!(fragments(&lines[4], TextRole::CodeString).contains("hello"));
        assert_eq!(lines[6].text, "╰─");
    }
    #[test]
    fn unknown_fence_is_readable_without_false_syntax() {
        let lines = render("```unknown-language\ngraph TD; A --> B\n```");
        assert_eq!(lines[1].text, "│ graph TD; A --> B");
        assert!(
            lines[1]
                .spans
                .iter()
                .all(|span| span.role == TextRole::CodeFence)
        );
    }
    #[test]
    fn user_underscores_and_escaped_markdown_remain_literal() {
        let lines = render(r"some_field_name and \*literal\* &amp; **real**");
        assert_eq!(lines[0].text, "some_field_name and *literal* & real");
        assert_eq!(fragments(&lines[0], TextRole::Strong), "real");
    }
}
