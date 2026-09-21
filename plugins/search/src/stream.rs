//! Exact direct-file scans and context expansion without loading whole files.
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::BufRead, path::Path};

const MAX_LINE_BYTES: usize = 10 * 1024 * 1024;
const DISPLAY_BYTES: usize = 512;

/// A single oversized line is diagnosed instead of growing the worker without bound.
fn visit_lines(
    path: &Path,
    capacity: usize,
    mut visit: impl FnMut(u64, &[u8], u64) -> bool,
) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut reader = std::io::BufReader::new(file);
    let mut prefix = Vec::new();
    let mut bytes = 0;
    let mut number = 1;
    loop {
        let available = reader.fill_buf().map_err(|e| e.to_string())?;
        if available.is_empty() {
            if bytes > 0 {
                visit(number, &prefix, bytes);
            }
            return Ok(());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let length = newline.unwrap_or(available.len());
        let keep = length.min(capacity - prefix.len());
        prefix.extend_from_slice(&available[..keep]);
        bytes += length as u64;
        reader.consume(length + usize::from(newline.is_some()));
        if newline.is_some() {
            if !visit(number, &prefix, bytes) {
                return Ok(());
            }
            prefix.clear();
            bytes = 0;
            number += 1;
        }
    }
}

pub(crate) struct Search {
    pub rows: Vec<Value>,
    pub skipped: Vec<Value>,
}
fn consumes_newline(hir: &regex_syntax::hir::Hir) -> bool {
    use regex_syntax::hir::{Class, HirKind};
    match hir.kind() {
        HirKind::Literal(value) => value.0.contains(&b'\n'),
        HirKind::Class(Class::Unicode(class)) => class
            .ranges()
            .iter()
            .any(|range| range.start() <= '\n' && '\n' <= range.end()),
        HirKind::Class(Class::Bytes(class)) => class
            .ranges()
            .iter()
            .any(|range| range.start() <= b'\n' && b'\n' <= range.end()),
        HirKind::Capture(value) => consumes_newline(&value.sub),
        HirKind::Repetition(value) => consumes_newline(&value.sub),
        HirKind::Concat(values) | HirKind::Alternation(values) => {
            values.iter().any(consumes_newline)
        }
        HirKind::Empty | HirKind::Look(_) => false,
    }
}
fn has_file_anchor(hir: &regex_syntax::hir::Hir) -> bool {
    use regex_syntax::hir::{HirKind, Look};
    match hir.kind() {
        HirKind::Look(Look::Start | Look::End) => true,
        HirKind::Capture(value) => has_file_anchor(&value.sub),
        HirKind::Repetition(value) => has_file_anchor(&value.sub),
        HirKind::Concat(values) | HirKind::Alternation(values) => {
            values.iter().any(has_file_anchor)
        }
        _ => false,
    }
}
pub(crate) fn search(
    path: &Path,
    relative: &str,
    pattern: &str,
    literal: bool,
    insensitive: bool,
) -> Result<Search, String> {
    let pattern = if literal {
        regex::escape(pattern)
    } else {
        pattern.into()
    };
    let hir = regex_syntax::ParserBuilder::new()
        .utf8(false)
        .multi_line(true)
        .case_insensitive(insensitive)
        .build()
        .parse(&pattern)
        .map_err(|e| format!("invalid regex: {e}"))?;
    let mut result = Search {
        rows: vec![],
        skipped: vec![],
    };
    if consumes_newline(&hir) {
        result.skipped.push(json!({
            "path": relative, "reason": "multiline_pattern_unsupported_for_large_file",
            "continuation": "Large explicit files support line-oriented exact matching; choose a pattern that cannot consume newlines.",
        }));
        return Ok(result);
    }
    if has_file_anchor(&hir) {
        result.skipped.push(json!({
            "path": relative, "reason": "file_anchor_unsupported_for_large_file",
            "continuation": "Large explicit files support line-oriented matching; file-boundary anchors are not reinterpreted as line boundaries.",
        }));
        return Ok(result);
    }
    let matcher = regex::bytes::RegexBuilder::new(&pattern)
        .case_insensitive(insensitive)
        .multi_line(true)
        .build()
        .map_err(|e| format!("invalid regex: {e}"))?;
    visit_lines(path, MAX_LINE_BYTES, |number, line, bytes| {
        if line.contains(&0) {
            result.rows.clear();
            result.skipped = vec![json!({ "path": relative, "reason": "binary" })];
            return false;
        }
        if bytes > MAX_LINE_BYTES as u64 {
            result.skipped.push(json!({
                "path": relative,
                "line": number,
                "reason": "line_size_limit",
                "line_bytes": bytes,
                "max_line_bytes": MAX_LINE_BYTES,
            }));
        } else if let Some(found) = matcher.find(line) {
            let mut row = line_row(relative, number, line, bytes);
            row["column"] = json!(found.start());
            row["score"] = Value::Null;
            row["definition_hint"] = json!(false);
            row["git_changed"] = json!(false);
            result.rows.push(row);
        }
        true
    })?;
    Ok(result)
}
fn line_row(path: &str, number: u64, line: &[u8], bytes: u64) -> Value {
    let bytes = bytes - u64::from(bytes == line.len() as u64 && line.ends_with(b"\r"));
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let prefix = &line[..line.len().min(DISPLAY_BYTES)];
    let mut text = String::from_utf8_lossy(prefix).into_owned();
    text.truncate(text.floor_char_boundary(DISPLAY_BYTES.min(text.len())));
    json!({
        "path": path,
        "line": number,
        "text": text,
        "line_bytes": bytes,
        "line_truncated": bytes > text.len() as u64,
    })
}

pub(crate) fn with_context(
    rows: Vec<Value>,
    root: &Path,
    context: u64,
) -> Result<Vec<Value>, String> {
    let mut files: Vec<(String, BTreeMap<u64, Option<Value>>)> = vec![];
    let mut positions = BTreeMap::new();
    for row in rows {
        let path = row["path"].as_str().ok_or("missing match path")?.to_owned();
        let number = row["line"].as_u64().ok_or("missing match line")?;
        let index = *positions.entry(path.clone()).or_insert_with(|| {
            files.push((path, BTreeMap::new()));
            files.len() - 1
        });
        let lines = &mut files[index].1;
        for number in number.saturating_sub(context).max(1)..=number.saturating_add(context) {
            lines.entry(number).or_insert(None);
        }
        lines.insert(number, Some(row));
    }
    let mut output = vec![];
    for (path, mut lines) in files {
        let last = *lines.last_key_value().ok_or("empty context range")?.0;
        visit_lines(&root.join(&path), DISPLAY_BYTES, |number, line, bytes| {
            if let Some(row) = lines.get_mut(&number)
                && row.is_none()
            {
                let mut context = line_row(&path, number, line, bytes);
                context["context"] = json!(true);
                *row = Some(context);
            }
            number < last
        })?;
        output.extend(lines.into_values().flatten());
    }
    Ok(output)
}
