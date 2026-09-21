//! Validate every edit against the original text before publishing one replacement.
use super::*;
use eden_plugin_sdk::serde_json::json;

struct Normalized {
    text: String,
    spans: Vec<(usize, usize, usize)>,
}

fn normalize(text: &str, tolerant: bool) -> Normalized {
    let mut result = Normalized {
        text: String::new(),
        spans: vec![],
    };
    let mut chars = text.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        let mut end = start + ch.len_utf8();
        let ch = if ch == '\r' && chars.peek().is_some_and(|(_, next)| *next == '\n') {
            chars.next();
            end += 1;
            '\n'
        } else {
            ch
        };
        if tolerant && matches!(ch, ' ' | '\t') {
            let mut tail = chars.clone();
            while tail
                .peek()
                .is_some_and(|(_, next)| matches!(next, ' ' | '\t'))
            {
                if let Some((position, whitespace)) = tail.next() {
                    end = position + whitespace.len_utf8();
                }
            }
            if tail
                .peek()
                .is_none_or(|(_, next)| matches!(next, '\r' | '\n'))
            {
                if let Some(last) = result.spans.last_mut() {
                    last.2 = end;
                }
                chars = tail;
                continue;
            }
            result
                .spans
                .push((result.text.len(), start, start + ch.len_utf8()));
            result.text.push(ch);
            while chars
                .peek()
                .is_some_and(|(_, next)| matches!(next, ' ' | '\t'))
            {
                if let Some((position, whitespace)) = chars.next() {
                    result.spans.push((
                        result.text.len(),
                        position,
                        position + whitespace.len_utf8(),
                    ));
                    result.text.push(whitespace);
                }
            }
            continue;
        }
        let ch = if tolerant {
            match ch {
                '\u{2018}' | '\u{2019}' => '\'',
                '\u{201c}' | '\u{201d}' => '"',
                '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2212}' => '-',
                other => other,
            }
        } else {
            ch
        };
        result.spans.push((result.text.len(), start, end));
        result.text.push(ch);
    }
    result
}

struct Replacement {
    start: usize,
    end: usize,
    text: String,
    index: usize,
}

pub(super) async fn edit(cwd: &Path, arguments: &Value) -> Result<ToolResult, Fault> {
    let mode = match arguments.get("mode") {
        None => "strict",
        Some(Value::String(mode)) if matches!(mode.as_str(), "strict" | "tolerant") => mode,
        _ => return Err(fault("InvalidInput", "mode must be strict or tolerant")),
    };
    let edits = if let Some(edits) = arguments.get("edits") {
        if arguments.get("old_text").is_some() || arguments.get("new_text").is_some() {
            return Err(fault(
                "InvalidInput",
                "use edits or old_text/new_text, not both",
            ));
        }
        let edits = edits
            .as_array()
            .filter(|edits| !edits.is_empty())
            .ok_or_else(|| fault("InvalidInput", "edits must be a nonempty array"))?;
        edits.iter().collect::<Vec<_>>()
    } else {
        vec![arguments]
    };
    let path = file_path(cwd, arguments)?;
    let original = tokio::fs::read_to_string(&path).await.map_err(file_error)?;
    let bom = if original.starts_with('\u{feff}') {
        3
    } else {
        0
    };
    let source = normalize(&original[bom..], mode == "tolerant");
    let crlf = original
        .find('\n')
        .is_some_and(|index| original.as_bytes().get(index.wrapping_sub(1)) == Some(&b'\r'));
    let mut replacements = vec![];
    for (index, edit) in edits.iter().enumerate() {
        let old = normalize(string(edit, "old_text")?, mode == "tolerant").text;
        if old.is_empty() {
            return Err(fault(
                "EditMismatch",
                "old_text must not normalize to empty; file unchanged",
            ));
        }
        let mut matches = source
            .text
            .char_indices()
            .filter_map(|(start, _)| source.text[start..].starts_with(&old).then_some(start));
        let start = matches.next().ok_or_else(|| {
            fault(
                "EditMismatch",
                format!("edit {index} has no {mode} match; file unchanged"),
            )
        })?;
        if matches.next().is_some() {
            return Err(fault(
                "EditMismatch",
                format!("edit {index} has multiple {mode} matches; file unchanged"),
            ));
        }
        let first = source
            .spans
            .binary_search_by_key(&start, |span| span.0)
            .map_err(|_| fault("EditMismatch", "invalid match boundary"))?;
        let end = start + old.len();
        let after = source.spans.partition_point(|span| span.0 < end);
        let new = string(edit, "new_text")?.replace("\r\n", "\n");
        replacements.push(Replacement {
            start: bom + source.spans[first].1,
            end: bom + source.spans[after - 1].2,
            text: if crlf { new.replace('\n', "\r\n") } else { new },
            index,
        });
    }
    replacements.sort_by_key(|edit| edit.start);
    if replacements
        .windows(2)
        .any(|pair| pair[0].end > pair[1].start)
    {
        return Err(fault(
            "EditMismatch",
            "edits overlap in original text; file unchanged",
        ));
    }
    let mut result_text = String::new();
    let mut details = vec![];
    let mut previous = 0;
    for edit in &replacements {
        result_text.push_str(&original[previous..edit.start]);
        result_text.push_str(&edit.text);
        details.push(json!({
            "index": edit.index,
            "start_byte": edit.start,
            "end_byte": edit.end,
            "start_line": original[..edit.start]
                    .bytes()
                    .filter(|b| *b == b'\n')
                    .count()
                    + 1,
            "end_line": original[..edit.end].bytes().filter(|b| *b == b'\n').count() + 1,
            "diff": { "before": &original[edit.start..edit.end], "after": edit.text },
        }));
        previous = edit.end;
    }
    result_text.push_str(&original[previous..]);
    tokio::fs::write(path, result_text)
        .await
        .map_err(file_error)?;
    let mut result = output(
        format!("Applied {} {mode} edit(s)", replacements.len()),
        None,
        false,
    );
    result.details = json!({ "mode": mode, "edits": details });
    Ok(result)
}
