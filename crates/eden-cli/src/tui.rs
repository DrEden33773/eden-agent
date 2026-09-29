//! Start or explicitly attach a terminal without placing rendering inside the CLI.
use crate::cli::Cli;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn state_dir() -> PathBuf {
    std::env::var_os("EDEN_TUI_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let root = std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os(if cfg!(windows) {
                        "LOCALAPPDATA"
                    } else {
                        "HOME"
                    })
                    .map(|p| PathBuf::from(p).join(".local/state"))
                })
                .unwrap_or_else(std::env::temp_dir);
            root.join("eden/tui")
        })
}
/// Attach a local terminal. The endpoint's session identity owns the recovered draft.
pub async fn attach(
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
    let identity = if frontend == "terminal" {
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
/// Create a detached host for a new task, or restore only the explicitly selected saved history.
pub async fn launch(
    cli: &Cli,
    endpoint: Option<&Path>,
    read: Option<&Path>,
    frontend: &str,
    editor: Option<&Path>,
) -> Result<i32, Box<dyn std::error::Error>> {
    if let Some(endpoint) = endpoint {
        return attach(endpoint, editor, frontend, monochrome(cli)).await;
    }
    let endpoint = start_host(cli, read)?;
    attach(&endpoint, editor, frontend, monochrome(cli)).await
}
/// Start a detached host for an explicitly selected saved session or reading artifact.
pub(crate) fn start_host(
    cli: &Cli,
    read: Option<&Path>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let directory = state_dir().join("hosts");
    std::fs::create_dir_all(&directory)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let endpoint = directory.join(format!("{stamp}-{}.json", std::process::id()));
    let log_path = endpoint.with_extension("log");
    let log = std::fs::File::create(&log_path)?;
    let mut command = Command::new(std::env::current_exe()?);
    for (flag, path) in [
        ("--composition", cli.composition.as_ref()),
        ("--cwd", cli.cwd.as_ref()),
        ("--global-dir", cli.global_dir.as_ref()),
        ("--session", cli.session.as_ref()),
    ] {
        if let Some(path) = path {
            command.arg(flag).arg(path);
        }
    }
    for path in &cli.env_file {
        command.arg("--env-file").arg(path);
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
    if let Some(path) = read {
        command.arg("read").arg(path);
    } else if cli.session.as_ref().is_some_and(|path| path.exists()) {
        command.arg("resume-live");
    } else {
        command.arg("live");
    }
    command
        .arg("--endpoint")
        .arg(&endpoint)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // A terminal detach must not send a shell hangup to the Session's host.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0200);
    }
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if endpoint.exists() {
            break;
        }
        if let Some(status) = child.try_wait()? {
            return Err(format!(
                "Host exited {status}: {}",
                std::fs::read_to_string(&log_path).unwrap_or_default()
            )
            .into());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("Host startup timed out; see {}", log_path.display()).into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(endpoint)
}

/// Explicit color selection takes precedence over the conventional environment override.
pub fn monochrome(cli: &Cli) -> bool {
    cli.color == "never"
        || (cli.color != "always"
            && std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()))
}
