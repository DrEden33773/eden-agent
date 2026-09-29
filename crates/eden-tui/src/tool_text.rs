//! Semantic presentation for tool labels and textual result previews.
use super::{RichLine, clean, code_lines, markdown, message_language};
use crate::model::{Message, Role, TextRole};

pub(super) fn tool_title(prefix: &str, title: &str, failed: bool, pending: bool) -> RichLine {
    let mut line = RichLine::default();
    line.push(prefix, &[TextRole::Muted]);
    if let Some(marker) = prefix.rfind(['✓', '✗', '◌', '⠿']) {
        line.spans.push(crate::model::TextSpan {
            range: marker..prefix.trim_end().len(),
            role: if failed {
                TextRole::ToolError
            } else if pending {
                TextRole::ToolName
            } else {
                TextRole::DiffAdded
            },
        });
    }
    let title = clean(title);
    let end = title.find(char::is_whitespace).unwrap_or(title.len());
    line.push(
        &title[..end],
        &[
            TextRole::Strong,
            if failed {
                TextRole::ToolError
            } else {
                TextRole::ToolName
            },
        ],
    );
    line.push(
        &title[end..],
        &[if title[..end].eq_ignore_ascii_case("multiple") {
            TextRole::Muted
        } else {
            TextRole::ToolTarget
        }],
    );
    line
}

pub(super) fn tool_output(title: &str, body: &str, width: usize) -> Vec<RichLine> {
    let body = clean(body);
    if body.contains("```") {
        return markdown(&body, width);
    }
    let kind = title
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let message = Message::new(0, Role::Tool, title, "");
    let language = message_language(&message);
    let mut output = Vec::new();
    for text in body.lines() {
        let mut line = RichLine::default();
        if matches!(text, "Input" | "Output" | "Matches" | "Entries") {
            line.push(text, &[TextRole::ToolName, TextRole::Strong]);
        } else if let Some((key, value)) = text.split_once(": ")
            && matches!(key, "path" | "command" | "pattern" | "glob" | "directory")
        {
            line.push(&format!("{key}: "), &[TextRole::Muted]);
            line.push(value, &[TextRole::InlineCode]);
        } else if kind == "read" && text.trim_start().starts_with(|c: char| c.is_ascii_digit()) {
            let start = text.len() - text.trim_start().len();
            let digits = text[start..]
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(text.len() - start);
            let end = start + digits;
            if text[end..].starts_with([' ', ':']) {
                line.push(&text[..end], &[TextRole::CodeNumber]);
                let suffix = &text[end..];
                for code in code_lines(suffix, language) {
                    line.append(&code);
                }
            } else {
                line.push(text, &[]);
            }
        } else if kind == "grep"
            && text
                .split(':')
                .nth(1)
                .is_some_and(|s| s.parse::<usize>().is_ok())
        {
            let mut fields = text.splitn(3, ':');
            line.push(fields.next().unwrap_or(""), &[TextRole::InlineCode]);
            line.push(":", &[TextRole::Muted]);
            line.push(fields.next().unwrap_or(""), &[TextRole::CodeNumber]);
            line.push(":", &[TextRole::Muted]);
            let result = fields.next().unwrap_or("");
            // The search expression is display metadata; literal matches need no regex execution.
            let query = title.split('"').nth(1).filter(|q| !q.is_empty());
            if let Some(query) = query {
                let mut from = 0;
                for (at, part) in result.match_indices(query) {
                    line.push(&result[from..at], &[]);
                    line.push(part, &[TextRole::Strong, TextRole::CodeNumber]);
                    from = at + part.len();
                }
                line.push(&result[from..], &[]);
            } else {
                line.push(result, &[]);
            }
        } else if (kind == "find" || kind == "ls") && !text.is_empty() && !text.contains(" · ") {
            let (path, meta) = text.split_once("  ").unwrap_or((text, ""));
            line.push(
                path,
                &[if path.ends_with('/') {
                    TextRole::ToolName
                } else {
                    TextRole::InlineCode
                }],
            );
            if !meta.is_empty() {
                line.push("  ", &[]);
                line.push(meta, &[TextRole::Muted]);
            }
        } else {
            line.push(text, &[]);
            // Make counts and execution metadata scannable without colouring the whole output.
            for (at, token) in text.match_indices(|c: char| c.is_ascii_digit()) {
                line.spans.push(crate::model::TextSpan {
                    range: at..at + token.len(),
                    role: TextRole::CodeNumber,
                });
            }
            if text.contains("not found")
                || text.contains("Permission denied")
                || text.starts_with("error:")
            {
                line.mark(TextRole::ToolError);
            }
        }
        output.push(line);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grep_preserves_code_indentation_and_highlights_only_literal_matches() {
        let source = "src/中文.py:48:    if ready:\nsrc/中文.py:49:        work()";
        let lines = tool_output("Grep \"ready\" src/", source, 80);
        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            source
        );
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.role == TextRole::Strong
                    && &lines[0].text[span.range.clone()] == "ready")
        );
    }
}
