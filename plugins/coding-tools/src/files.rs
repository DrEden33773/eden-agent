//! File reads preserve recoverable text positions and self-contained image bytes.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use eden_plugin_sdk::{protocol::coding::Block, serde_json::json};

const IMAGE_LIMIT: usize = 10 * 1024 * 1024;

pub(super) async fn read(cwd: &Path, arguments: &Value) -> Result<ToolResult, Fault> {
    let path = file_path(cwd, arguments)?;
    let bytes = tokio::fs::read(path).await.map_err(file_error)?;
    if let Some(media_type) = image_type(&bytes) {
        if bytes.len() > IMAGE_LIMIT {
            return Err(fault("InvalidInput", "image exceeds the 10 MiB read limit"));
        }
        if ["offset", "limit", "byte_offset"]
            .iter()
            .any(|key| arguments.get(key).is_some())
        {
            return Err(fault(
                "InvalidInput",
                "image reads do not accept text ranges",
            ));
        }
        let mut result = output(
            format!("Read {} image bytes ({media_type})", bytes.len()),
            None,
            false,
        );
        result.content.push(Block::Image {
            media_type: media_type.into(),
            data: STANDARD.encode(&bytes),
        });
        result.details =
            json!({ "complete": true, "bytes": bytes.len(), "media_type": media_type });
        return Ok(result);
    }
    let content = std::str::from_utf8(&bytes)
        .map_err(|_| fault("FileFailure", "file is not UTF-8 text or a supported image"))?;
    read_text(content, arguments)
}

fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

fn read_text(content: &str, arguments: &Value) -> Result<ToolResult, Fault> {
    let offset = integer(arguments, "offset", 1)?;
    let limit = integer(arguments, "limit", u64::MAX)?;
    let byte_offset = arguments
        .get("byte_offset")
        .map(|v| {
            v.as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| fault("InvalidInput", "byte_offset must be a nonnegative integer"))
        })
        .transpose()?
        .unwrap_or(0);
    let lines: Vec<_> = content.split_inclusive('\n').collect();
    if offset > (lines.len() as u64).max(1) {
        return Err(fault("InvalidInput", "offset is beyond the last line"));
    }
    let start = (offset - 1) as usize;
    let line = lines.get(start).copied().unwrap_or("");
    if !line.is_char_boundary(byte_offset) || (byte_offset > 0 && byte_offset >= line.len()) {
        return Err(fault(
            "InvalidInput",
            "byte_offset must point inside the selected line at a UTF-8 boundary",
        ));
    }
    let mut text = String::new();
    let mut next_line = start;
    let mut next_byte = byte_offset;
    let mut partial_line = false;
    let mut truncated = false;
    for (index, line) in lines
        .iter()
        .enumerate()
        .skip(start)
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
    {
        let begin = if index == start { byte_offset } else { 0 };
        let remaining = &line[begin..];
        if text.len() + remaining.len() > OUTPUT_LIMIT {
            truncated = true;
            if !text.is_empty() {
                break;
            }
            let end = remaining.floor_char_boundary(OUTPUT_LIMIT);
            text.push_str(&remaining[..end]);
            next_line = index;
            next_byte = begin + end;
            partial_line = true;
            break;
        }
        text.push_str(remaining);
        next_line = index + 1;
        next_byte = 0;
    }
    let complete = next_line == lines.len();
    let mut result = output(text, None, truncated);
    result.details = json!({
        "offset": offset,
        "byte_offset": byte_offset,
        "total_lines": lines.len(),
        "complete": complete,
        "partial_line": partial_line,
        "next_offset": if complete {
                None
            } else {
                Some(next_line + 1)
            },
        "next_byte_offset": if complete {
                None
            } else {
                Some(next_byte)
            },
    });
    Ok(result)
}
