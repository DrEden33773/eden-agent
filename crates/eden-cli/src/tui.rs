//! CLI consumers delegate Session ownership and discovery to the shared lifecycle service.
use crate::cli::Cli;
pub(crate) use eden_session_workspace::Registration;
use eden_session_workspace::state_dir;
use eden_session_workspace::{Cleanup, Lifecycle};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

/// Capture launch settings independently of any user Session.
pub fn lifecycle(cli: &Cli) -> Result<Lifecycle, Box<dyn std::error::Error>> {
    let invocation = std::env::current_dir()?;
    let mut command = Command::new(std::env::current_exe()?);
    {
        for (flag, path) in [
            ("--composition", cli.composition.as_ref()),
            ("--global-dir", cli.global_dir.as_ref()),
        ] {
            if let Some(path) = path {
                command.arg(flag).arg(invocation.join(path));
            }
        }
        for path in &cli.env_file {
            command.arg("--env-file").arg(invocation.join(path));
        }
        for (on, flag) in [
            (cli.trust_project, "--trust-project"),
            (cli.no_trust_project, "--no-trust-project"),
            (cli.offline_startup, "--offline-startup"),
            (cli.no_update_check, "--no-update-check"),
            (cli.no_session, "--no-session"),
            (cli.read_only, "--read-only"),
            (cli.no_context, "--no-context"),
            (cli.no_skills, "--no-skills"),
            (cli.no_templates, "--no-templates"),
        ] {
            if on {
                command.arg(flag);
            }
        }
        for (flag, values) in [
            ("--tools", &cli.tools),
            ("--exclude-tools", &cli.exclude_tools),
        ] {
            if !values.is_empty() {
                command.arg(format!("{flag}={}", values.join(",")));
            }
        }
        for (flag, values) in [
            ("--skill-path", &cli.skill_path),
            ("--template-path", &cli.template_path),
        ] {
            for path in values {
                command.arg(flag).arg(path);
            }
        }
        if let Some(model) = &cli.model {
            command.arg("--model").arg(model);
            if let Some(thinking) = &cli.thinking {
                command.arg("--thinking").arg(thinking);
            }
        }
    }

    Ok(Lifecycle::new(
        std::env::current_exe()?,
        command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        std::fs::canonicalize(
            cli.cwd
                .as_ref()
                .cloned()
                .unwrap_or(std::env::current_dir()?),
        )?,
        state_dir(),
        std::env::var_os("EDEN_LEGACY_SESSION_DIRS")
            .map(|value| {
                std::env::split_paths(&value)
                    .map(|path| invocation.join(path))
                    .collect()
            })
            .unwrap_or_default(),
    ))
}

/// Launch the installed frontend after explicit lifecycle selection.
pub async fn launch(
    cli: &Cli,
    endpoint: Option<&Path>,
    read: Option<&Path>,
    frontend: &str,
    editor: Option<&Path>,
) -> Result<i32, Box<dyn std::error::Error>> {
    let lifecycle = lifecycle(cli)?;
    if editor.is_none() && frontend == "terminal" {
        let target = if let Some(endpoint) = endpoint {
            eden_session_workspace::OpenTarget::Endpoint(endpoint.to_owned())
        } else if let Some(path) = read {
            eden_session_workspace::OpenTarget::History {
                path: path.to_owned(),
                reading: true,
            }
        } else if let Some(path) = &cli.session {
            eden_session_workspace::OpenTarget::History {
                path: path.clone(),
                reading: cli.read_only,
            }
        } else {
            eden_session_workspace::OpenTarget::New {
                persist: !cli.no_session,
            }
        };
        return eden_terminal::run(lifecycle, target, monochrome(cli))
            .await
            .map_err(|error| format!("{error:#}").into());
    }
    let opened = if let Some(endpoint) = endpoint {
        lifecycle.attach(endpoint, None).await?
    } else if let Some(path) = read {
        lifecycle.open(path, true).await?
    } else if let Some(path) = &cli.session {
        lifecycle.open(path, cli.read_only).await?
    } else {
        lifecycle.create(!cli.no_session).await?
    };
    if editor.is_some() || frontend != "terminal" {
        let result = attach_native(&opened.endpoint, editor, frontend, monochrome(cli)).await;
        if opened.cleanup == Cleanup::OwnedReader {
            lifecycle.close(&opened).await?;
        }
        return result;
    }
    unreachable!("terminal library path handled above")
}
/// Attach the installed frontend to an explicitly validated owner; it is always borrowed.
pub async fn attach(
    endpoint: &Path,
    editor: Option<&Path>,
    frontend: &str,
    no_color: bool,
) -> Result<i32, Box<dyn std::error::Error>> {
    if editor.is_some() || frontend != "terminal" {
        return attach_native(endpoint, editor, frontend, no_color).await;
    }
    let args = vec![std::ffi::OsString::from("eden")];
    let parsed = crate::cli::parse(&args, clap::ColorChoice::Never);
    let lifecycle = lifecycle(&parsed.cli)?;
    eden_terminal::run(
        lifecycle,
        eden_session_workspace::OpenTarget::Endpoint(endpoint.to_owned()),
        no_color,
    )
    .await
    .map_err(|error| format!("{error:#}").into())
}
/// Attach a local terminal. The endpoint's session identity owns the recovered draft.
async fn attach_native(
    endpoint: &Path,
    editor: Option<&Path>,
    frontend: &str,
    no_color: bool,
) -> Result<i32, Box<dyn std::error::Error>> {
    let editor = editor
        .map(Path::to_owned)
        .or_else(|| std::env::var_os("EDEN_TUI_EDITOR").map(PathBuf::from))
        .unwrap_or_else(|| {
            let executable = std::env::current_exe().unwrap_or_default();
            executable
                .parent()
                .and_then(Path::parent)
                .unwrap_or(Path::new("."))
                .join("ui")
                .join(format!(
                    "{}eden_terminal_editor{}",
                    std::env::consts::DLL_PREFIX,
                    std::env::consts::DLL_SUFFIX
                ))
        });
    let identity = if frontend == "terminal" || frontend == "native" {
        let source = std::env::var("WT_SESSION")
            .or_else(|_| std::env::var("TERM_SESSION_ID"))
            .ok()
            .or_else(|| {
                std::fs::read_link("/proc/self/fd/0")
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| format!("process-{}", std::process::id()));
        format!(
            "terminal-{}",
            source
                .as_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )
    } else {
        frontend.to_owned()
    };
    eden_tui::run(eden_tui::Options {
        endpoint: endpoint.into(),
        editor,
        state_dir: state_dir(),
        frontend: identity,
        no_color,
    })
    .await
}
/// Explicit color selection takes precedence over the conventional environment override.
pub fn monochrome(cli: &Cli) -> bool {
    cli.color == "never"
        || (cli.color != "always"
            && std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()))
}

/// Compatibility callers share exactly the same owner verification policy.
pub(crate) async fn selected_live_endpoint(
    history: &Path,
) -> Result<Option<PathBuf>, eden_protocol::Fault> {
    let parsed = crate::cli::parse(&["eden".into()], clap::ColorChoice::Never);
    let lifecycle = lifecycle(&parsed.cli).map_err(|e| {
        eden_protocol::Fault::new("StartFailed", "session-lifecycle", e.to_string())
    })?;
    Ok(lifecycle
        .owner(history)
        .await?
        .map(|opened| opened.endpoint))
}
