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

pub(super) async fn write(
    cwd: &Path,
    arguments: &Value,
    artifact_dir: &Path,
) -> Result<ToolResult, Fault> {
    let path = file_path(cwd, arguments)?;
    let content = string(arguments, "content")?;
    let (before, created) = match tokio::fs::read(&path).await {
        Ok(bytes) => (bytes, false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (vec![], true),
        Err(error) => return Err(file_error(error)),
    };
    let before_text = std::str::from_utf8(&before).ok();
    let truncated =
        before.len() > OUTPUT_LIMIT || content.len() > OUTPUT_LIMIT || before_text.is_none();
    let retained = if truncated {
        Some(retain_write_sides(artifact_dir, &before, content.as_bytes()).await?)
    } else {
        None
    };
    let changed = async {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(file_error)?;
        }
        tokio::fs::write(&path, content).await.map_err(file_error)
    }
    .await;
    if let Err(error) = changed {
        if let Some((directory, _)) = &retained {
            let _ = tokio::fs::remove_dir_all(directory).await;
        }
        return Err(error);
    }
    let mut result = output(format!("Wrote {} bytes", content.len()), None, truncated);
    result.details = json!({
        "created": created,
        "diff": {
            "before": before_text.map(|text| &text[..text.floor_char_boundary(OUTPUT_LIMIT)]),
            "after": &content[..content.floor_char_boundary(OUTPUT_LIMIT)],
            "complete": !truncated,
            "before_bytes": before.len(),
            "after_bytes": content.len(),
        },
    });
    if let Some((_, artifacts)) = retained {
        result.artifacts = artifacts;
    }
    Ok(result)
}

async fn retain_write_sides(
    root: &Path,
    before: &[u8],
    after: &[u8],
) -> Result<(PathBuf, Vec<eden_plugin_sdk::protocol::coding::Artifact>), Fault> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::AsyncWriteExt;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    tokio::fs::create_dir_all(root).await.map_err(file_error)?;
    let root = tokio::fs::canonicalize(root).await.map_err(file_error)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| fault("FileFailure", error.to_string()))?
        .as_nanos();
    let directory = root.join(format!(
        "write-{}-{stamp}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&directory).map_err(file_error)?;
    let result = async {
        let mut artifacts = vec![];
        for (name, bytes) in [("before", before), ("after", after)] {
            let path = directory.join(format!("{name}.bin"));
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                options.mode(0o600);
            }
            let mut file = options.open(&path).await.map_err(file_error)?;
            file.write_all(bytes).await.map_err(file_error)?;
            file.flush().await.map_err(file_error)?;
            file.sync_all().await.map_err(file_error)?;
            artifacts.push(eden_plugin_sdk::protocol::coding::Artifact {
                path: path.to_string_lossy().into_owned(),
                name: name.into(),
                bytes: bytes.len() as u64,
                media_type: if std::str::from_utf8(bytes).is_ok() {
                    "text/plain"
                } else {
                    "application/octet-stream"
                }
                .into(),
            });
        }
        Ok(artifacts)
    }
    .await;
    match result {
        Ok(artifacts) => Ok((directory, artifacts)),
        Err(error) => {
            let _ = tokio::fs::remove_dir_all(directory).await;
            Err(error)
        }
    }
}
