//! Default file and shell tools, with operation-owned side effects.
use eden_plugin_sdk::{
    Package, Scope,
    protocol::{
        Descriptor, Fault,
        coding::{TOOL, ToolRequest, ToolResult},
    },
    serde_json::Value,
};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncReadExt};

mod catalog;

mod registry;

const OUTPUT_LIMIT: usize = 65_536;

fn descriptor() -> Descriptor {
    Descriptor {
        package: "coding-tools".into(),
        version: "0.1.0".into(),
        provides: vec![
            eden_plugin_sdk::protocol::resources::TOOL_CATALOG.into(),
            TOOL.into(),
        ],
    }
}
fn create(config: Value) -> Result<Package, Fault> {
    use eden_plugin_sdk::protocol::resources as r;
    let registry = registry::Registry::new(&config)?;
    let listing = registry.clone();
    let read_observer = config["read_observer"].as_str().map(String::from);
    let shell = match config.get("bash") {
        None => "bash".into(),
        Some(Value::String(path)) if !path.is_empty() => path.clone(),
        Some(_) => {
            return Err(fault(
                "InvalidInput",
                "bash configuration must be a nonempty executable path",
            ));
        }
    };
    let powershell = match config.get("powershell") {
        None => "pwsh".into(),
        Some(Value::String(path)) if !path.is_empty() => path.clone(),
        Some(_) => {
            return Err(fault(
                "InvalidInput",
                "powershell configuration must be a nonempty executable path",
            ));
        }
    };
    Ok(Package::new("coding-tools")
        .service(r::TOOL_CATALOG, move |request: r::CatalogRequest, cx| {
            let registry = listing.clone();
            async move {
                Ok(r::Catalog {
                    tools: registry
                        .catalog(&cx, &request.cwd)
                        .await?
                        .into_values()
                        .map(|(tool, _)| tool)
                        .collect(),
                })
            }
        })
        .service(TOOL, move |request: ToolRequest, cx| {
            let registry = registry.clone();
            let shell = if request.name == "powershell" {
                powershell.clone()
            } else {
                shell.clone()
            };
            let read_observer = read_observer.clone();
            async move {
                let selected = registry.catalog(&cx, &request.cwd).await?;
                let (_, route) = selected.get(&request.name).ok_or_else(|| {
                    fault(
                        "UnknownTool",
                        format!("tool is not enabled: {}", request.name),
                    )
                })?;
                if let Some(route) = route {
                    return cx.call(route, &request).await;
                }
                if request.name == "skill" {
                    let name = string(&request.arguments, "name")?;
                    let state: r::ResourceReply =
                        cx.call(r::SOURCE, &r::ResourceRequest::Snapshot).await?;
                    if !state
                        .snapshot
                        .skills
                        .iter()
                        .any(|skill| skill.name == name && skill.model_invocable)
                    {
                        return Err(fault(
                            "InvalidInput",
                            "skill is unavailable for model invocation",
                        ));
                    }
                    let reply: r::ResourceReply = cx
                        .call(
                            r::SOURCE,
                            &r::ResourceRequest::Skill {
                                name: name.into(),
                                arguments: request
                                    .arguments
                                    .get("arguments")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .into(),
                            },
                        )
                        .await?;
                    return Ok(output(reply.text.unwrap_or_default(), None, false));
                }
                let observed = if request.name == "read" {
                    Some(request.clone())
                } else {
                    None
                };
                let result = execute(request, cx.scope.clone(), shell).await?;
                if result.error.is_none()
                    && let (Some(route), Some(request)) = (read_observer, observed)
                {
                    let _: () = cx.call(&route, &request).await?;
                }
                Ok(result)
            }
        }))
}
eden_plugin_sdk::export_plugin!(descriptor, create);

async fn execute(request: ToolRequest, scope: Scope, shell: String) -> Result<ToolResult, Fault> {
    let cancellation = scope.cancellation();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    // The SDK drops a cancelled root future. The admitted child owns all file
    // operations and process cleanup until their side effects have settled.
    scope.spawn(async move {
        let result = if tokio::select! {
            biased;
            _ = cancellation.cancelled() => true,
            _ = std::future::ready(()) => false,
        } {
            Err(fault("Cancelled", "tool cancelled before execution"))
        } else {
            run(request, shell, cancellation).await
        };
        let result = result.unwrap_or_else(failed);
        let cleanup = result
            .error
            .as_ref()
            .filter(|error| error.code == "CleanupFailure")
            .cloned();
        let _ = sender.send(result);
        match cleanup {
            Some(error) => Err(error),
            None => Ok(()),
        }
    })?;
    receiver
        .await
        .map_err(|_| fault("ToolFailure", "tool completion lost"))
}

async fn run(
    request: ToolRequest,
    shell: String,
    cancellation: eden_plugin_sdk::Cancellation,
) -> Result<ToolResult, Fault> {
    let cwd = Path::new(&request.cwd);
    if !cwd.is_absolute()
        || !tokio::fs::metadata(cwd)
            .await
            .map(|meta| meta.is_dir())
            .unwrap_or(false)
    {
        return Err(fault(
            "InvalidInput",
            "cwd must be an existing absolute directory",
        ));
    }
    match request.name.as_str() {
        "ls" => {
            let path = cwd.join(
                request
                    .arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("."),
            );
            let mut reader = tokio::fs::read_dir(&path).await.map_err(file_error)?;
            let mut entries = vec![];
            while let Some(entry) = reader.next_entry().await.map_err(file_error)? {
                let suffix = if entry.file_type().await.map_err(file_error)?.is_dir() {
                    "/"
                } else {
                    ""
                };
                entries.push(format!("{}{suffix}", entry.file_name().to_string_lossy()));
            }
            entries.sort();
            Ok(output(entries.join("\n"), None, false))
        }
        "read" => {
            let path = file_path(cwd, &request.arguments)?;
            let offset = integer(&request.arguments, "offset", 1)?;
            let limit = integer(&request.arguments, "limit", u64::MAX)?;
            let content = tokio::fs::read_to_string(path).await.map_err(file_error)?;
            let lines = content.split_inclusive('\n');
            let count = lines.clone().count() as u64;
            if offset > count.max(1) {
                return Err(fault("InvalidInput", "offset is beyond the last line"));
            }
            let mut text = String::new();
            let mut truncated = false;
            for line in lines
                .skip((offset - 1) as usize)
                .take(usize::try_from(limit).unwrap_or(usize::MAX))
            {
                if text.len() + line.len() > OUTPUT_LIMIT {
                    let remaining = OUTPUT_LIMIT - text.len();
                    text.push_str(&line[..line.floor_char_boundary(remaining)]);
                    truncated = true;
                    break;
                }
                text.push_str(line);
            }
            Ok(ToolResult {
                text,
                truncated,
                exit_code: None,
                error: None,
            })
        }
        "write" => {
            let path = file_path(cwd, &request.arguments)?;
            let content = string(&request.arguments, "content")?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(file_error)?;
            }
            tokio::fs::write(path, content).await.map_err(file_error)?;
            Ok(output(
                format!("Wrote {} bytes", content.len()),
                None,
                false,
            ))
        }
        "edit" => {
            let path = file_path(cwd, &request.arguments)?;
            let old = string(&request.arguments, "old_text")?;
            let new = string(&request.arguments, "new_text")?;
            if old.is_empty() {
                return Err(fault("EditMismatch", "old_text must not be empty"));
            }
            let content = tokio::fs::read_to_string(&path).await.map_err(file_error)?;
            let start = content.find(old).ok_or_else(|| {
                fault(
                    "EditMismatch",
                    "old_text has no exact match; file unchanged",
                )
            })?;
            let next = start + old.chars().next().map(char::len_utf8).unwrap_or(0);
            if content[next..].contains(old) {
                return Err(fault(
                    "EditMismatch",
                    "old_text has multiple exact matches; file unchanged",
                ));
            }
            let mut replacement = content[..start].to_owned();
            replacement.push_str(new);
            replacement.push_str(&content[start + old.len()..]);
            tokio::fs::write(path, replacement)
                .await
                .map_err(file_error)?;
            Ok(output("Replaced one exact match".into(), None, false))
        }
        "bash" | "powershell" => {
            shell_command(
                cwd,
                string(&request.arguments, "command")?,
                &shell,
                request.name == "powershell",
                cancellation,
            )
            .await
        }
        _ => Err(fault(
            "UnknownTool",
            format!("unknown tool: {}", request.name),
        )),
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, Fault> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| fault("InvalidInput", format!("{key} must be a string")))
}
fn integer(value: &Value, key: &str, default: u64) -> Result<u64, Fault> {
    match value.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|value| *value > 0)
            .ok_or_else(|| fault("InvalidInput", format!("{key} must be a positive integer"))),
    }
}
fn file_path(cwd: &Path, value: &Value) -> Result<PathBuf, Fault> {
    let path = string(value, "path")?;
    if path.is_empty() {
        return Err(fault("InvalidInput", "path must not be empty"));
    }
    Ok(cwd.join(path))
}
fn fault(code: &str, message: impl Into<String>) -> Fault {
    Fault::new(code, "coding-tools", message)
}
fn file_error(error: std::io::Error) -> Fault {
    fault("FileFailure", error.to_string())
}
fn failed(error: Fault) -> ToolResult {
    ToolResult {
        text: error.message.clone(),
        exit_code: None,
        truncated: false,
        error: Some(error),
    }
}
fn output(mut text: String, exit_code: Option<i32>, mut truncated: bool) -> ToolResult {
    if text.len() > OUTPUT_LIMIT {
        text.truncate(text.floor_char_boundary(OUTPUT_LIMIT));
        truncated = true;
    }
    ToolResult {
        text,
        exit_code,
        truncated,
        error: None,
    }
}

async fn capture(mut pipe: impl AsyncRead + Unpin) -> std::io::Result<(Vec<u8>, bool)> {
    let mut captured = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        let read = pipe.read(&mut chunk).await?;
        if read == 0 {
            return Ok((captured, truncated));
        }
        let keep = read.min(OUTPUT_LIMIT - captured.len());
        captured.extend_from_slice(&chunk[..keep]);
        truncated |= keep < read;
    }
}

async fn shell_command(
    cwd: &Path,
    command: &str,
    shell: &str,
    powershell: bool,
    cancellation: eden_plugin_sdk::Cancellation,
) -> Result<ToolResult, Fault> {
    let args: &[&str] = if powershell {
        &[
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            command,
        ]
    } else {
        &["--noprofile", "--norc", "-c", command]
    };
    let (mut child, tree) = eden_process::spawn(shell, args, cwd).await?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| fault("ToolFailure", "stdout pipe missing"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| fault("ToolFailure", "stderr pipe missing"))?;
    let wait = async {
        let (status, cancelled) = tokio::select! {
            status = child.wait() => (status, false),
            _ = cancellation.cancelled() => {
                tree.terminate()?;
                (child.wait().await, true)
            }
        };
        // Foreground completion cannot leave background children holding pipes
        // or continuing file writes after the tool result has been committed.
        tree.terminate()?;
        tree.settle().await?;
        Ok::<_, Fault>((
            status.map_err(|error| fault("ToolFailure", error.to_string()))?,
            cancelled,
        ))
    };
    let (waited, stdout, stderr) = tokio::join!(wait, capture(stdout), capture(stderr));
    let (status, cancelled) = waited?;
    let (stdout, out_truncated) = stdout.map_err(file_error)?;
    let (stderr, err_truncated) = stderr.map_err(file_error)?;
    let mut text = String::from_utf8_lossy(&stdout).into_owned();
    if !stderr.is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(&String::from_utf8_lossy(&stderr));
    }
    let mut result = output(text, status.code(), out_truncated || err_truncated);
    result.error = if cancelled {
        Some(fault(
            "Cancelled",
            "shell command cancelled; process tree stopped",
        ))
    } else if !status.success() {
        Some(fault("ShellExit", format!("shell exited with {status}")))
    } else {
        None
    };
    Ok(result)
}

#[cfg(test)]
mod tests;
