//! Default file and shell tools, with operation-owned side effects.
use eden_plugin_sdk::{
    Package, Scope,
    protocol::{
        Descriptor, Fault,
        coding::{TOOL, ToolRequest, ToolResult},
    },
    serde_json::{self, Value},
};
use std::path::{Path, PathBuf};

mod catalog;
mod edit;
mod files;
mod shell;

mod registry;

const OUTPUT_LIMIT: usize = 65_536;

fn descriptor() -> Descriptor {
    Descriptor {
        package: "coding-tools".into(),
        version: "0.1.0".into(),
        provides: vec![
            eden_plugin_sdk::protocol::resources::TOOL_CATALOG.into(),
            TOOL.into(),
            eden_plugin_sdk::protocol::shell::USER_SHELL.into(),
        ],
    }
}
fn create(mut config: Value) -> Result<Package, Fault> {
    if let Some(host) =
        eden_plugin_sdk::protocol::environment::HostEnvironment::from_config(&config)?
    {
        config["artifact_dir"] = serde_json::json!(host.global_dir.join("artifacts"));
        for key in ["tools", "exclude_tools", "read_only"] {
            if let Some(value) = host.settings.get(key) {
                config[key] = value.clone();
            }
        }
    }
    use eden_plugin_sdk::protocol::resources as r;
    let registry = registry::Registry::new(&config)?;
    let listing = registry.clone();
    let read_observer = config["read_observer"].as_str().map(String::from);
    let artifact_dir = config
        .get("artifact_dir")
        .map(|value| {
            value
                .as_str()
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .ok_or_else(|| fault("InvalidInput", "artifact_dir must be a nonempty path"))
        })
        .transpose()?
        .unwrap_or_else(|| {
            eden_workspace::WorkspaceOptions::default()
                .global_dir
                .join("artifacts")
        });
    if !artifact_dir.is_absolute() {
        return Err(fault("InvalidInput", "artifact_dir must be absolute"));
    }
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
    let user_bash = shell.clone();
    let user_powershell = powershell.clone();
    let user_artifact_dir = artifact_dir.clone();
    let default_bash = config.get("bash").is_none();
    let default_powershell = config.get("powershell").is_none();
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
            let default_shell = if request.name == "powershell" {
                default_powershell
            } else {
                default_bash
            };
            let read_observer = read_observer.clone();
            let artifact_dir = artifact_dir.clone();
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
                if let Some(name) = resource_skill(&request)? {
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
                let shell = if matches!(request.name.as_str(), "bash" | "powershell") {
                    let cwd = Path::new(&request.cwd);
                    let configured =
                        if shell.starts_with('~') || Path::new(&shell).components().count() > 1 {
                            eden_workspace::paths::resolve_path(cwd, Path::new(&shell))?
                        } else {
                            PathBuf::from(&shell)
                        };
                    let configured = configured.to_str().ok_or_else(|| {
                        fault("ShellUnavailable", "shell executable path is not Unicode")
                    })?;
                    let kind = if request.name == "powershell" {
                        eden_process::ShellKind::PowerShell
                    } else {
                        eden_process::ShellKind::Bash
                    };
                    let resolved =
                        eden_process::resolve_shell(configured, kind, cwd, default_shell)?;
                    resolved
                        .executable
                        .to_str()
                        .ok_or_else(|| {
                            fault(
                                "ShellUnavailable",
                                "resolved shell executable path is not Unicode",
                            )
                        })?
                        .to_owned()
                } else {
                    shell
                };
                let result =
                    execute_with_artifacts(request, cx.scope.clone(), shell, artifact_dir).await?;
                if result.error.is_none()
                    && let (Some(route), Some(request)) = (read_observer, observed)
                {
                    let _: () = cx.call(&route, &request).await?;
                }
                Ok(result)
            }
        })
        .service(
            eden_plugin_sdk::protocol::shell::USER_SHELL,
            move |request, cx| {
                shell::user(
                    request,
                    cx,
                    (user_bash.clone(), default_bash),
                    (user_powershell.clone(), default_powershell),
                    user_artifact_dir.clone(),
                )
            },
        ))
}
eden_plugin_sdk::export_plugin!(descriptor, create);

fn resource_skill(request: &ToolRequest) -> Result<Option<&str>, Fault> {
    match request.name.as_str() {
        "skill" => string(&request.arguments, "name").map(Some),
        "read" => Ok(string(&request.arguments, "path")?.strip_prefix("eden-resource://skill/")),
        _ => Ok(None),
    }
}

#[cfg(test)]
async fn execute(request: ToolRequest, scope: Scope, shell: String) -> Result<ToolResult, Fault> {
    let artifact_dir = Path::new(&request.cwd).join(".test-artifacts");
    execute_with_artifacts(request, scope, shell, artifact_dir).await
}

async fn execute_with_artifacts(
    request: ToolRequest,
    scope: Scope,
    shell: String,
    artifact_dir: PathBuf,
) -> Result<ToolResult, Fault> {
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
            run(request, shell, artifact_dir, cancellation).await
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
    artifact_dir: PathBuf,
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
        "read" => files::read(cwd, &request.arguments).await,
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
        "edit" => edit::edit(cwd, &request.arguments).await,
        "bash" | "powershell" => {
            shell::run(
                cwd,
                string(&request.arguments, "command")?,
                &shell,
                request.name == "powershell",
                shell::timeout(&request.arguments)?,
                &artifact_dir,
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
    eden_workspace::paths::resolve_path(cwd, Path::new(path))
}
fn fault(code: &str, message: impl Into<String>) -> Fault {
    Fault::new(code, "coding-tools", message)
}
fn file_error(error: std::io::Error) -> Fault {
    fault("FileFailure", error.to_string())
}
fn failed(error: Fault) -> ToolResult {
    ToolResult {
        content: vec![],
        details: serde_json::Value::Null,
        artifacts: vec![],
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
        content: vec![],
        details: serde_json::Value::Null,
        artifacts: vec![],
        text,
        exit_code,
        truncated,
        error: None,
    }
}

#[cfg(test)]
mod tests;
