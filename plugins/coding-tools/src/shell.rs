//! Shell output retention and per-command deadlines share the process-domain barrier.
use super::{Fault, OUTPUT_LIMIT, ToolResult, Value, fault, file_error, serde_json};
use eden_plugin_sdk::{
    CallContext,
    protocol::{
        coding::Artifact,
        shell::{BEFORE_USER_SHELL, ShellHookReply, ShellOutput, ShellRequest},
    },
};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

const PREVIEW_LIMIT: usize = (OUTPUT_LIMIT - 128) / 2;
static NEXT_OUTPUT: AtomicU64 = AtomicU64::new(0);

pub(super) async fn user(
    mut request: ShellRequest,
    cx: CallContext,
    bash: (String, bool),
    powershell: (String, bool),
    artifact_dir: PathBuf,
) -> Result<ToolResult, Fault> {
    validate_user_request(&request).await?;
    let hook: Option<ShellHookReply> = match cx.call(BEFORE_USER_SHELL, &request).await {
        Ok(reply) => Some(reply),
        Err(error)
            if error.code == "MissingDependency"
                && error.source == "router"
                && error.message == BEFORE_USER_SHELL =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    match hook {
        Some(ShellHookReply::Execute { request: changed }) => {
            if changed.cwd != request.cwd {
                return Err(fault(
                    "InvalidInput",
                    "user shell hook cannot change session cwd",
                ));
            }
            request = changed;
            validate_user_request(&request).await?;
        }
        Some(ShellHookReply::Return { mut result }) => {
            annotate(&mut result, &request, true);
            return Ok(result);
        }
        None => {}
    }
    let (configured, default_shell) = if request.shell == "powershell" {
        powershell
    } else {
        bash
    };
    let cwd = Path::new(&request.cwd);
    let configured =
        if configured.starts_with('~') || Path::new(&configured).components().count() > 1 {
            eden_workspace::paths::resolve_path(cwd, Path::new(&configured))?
        } else {
            PathBuf::from(configured)
        };
    let configured = configured
        .to_str()
        .ok_or_else(|| fault("ShellUnavailable", "shell executable path is not Unicode"))?;
    let kind = if request.shell == "powershell" {
        eden_process::ShellKind::PowerShell
    } else {
        eden_process::ShellKind::Bash
    };
    let resolved = eden_process::resolve_shell(configured, kind, cwd, default_shell)?;
    let executable = resolved
        .executable
        .to_str()
        .ok_or_else(|| {
            fault(
                "ShellUnavailable",
                "resolved shell executable path is not Unicode",
            )
        })?
        .to_owned();
    let cancellation = cx.scope.cancellation();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let scope = cx.scope.clone();
    // Retain process and pipe ownership after the service root is cancelled.
    let capture = scope.clone();
    scope.spawn(async move {
        let execution = async {
            if cancellation.is_cancelled() {
                return Err(fault("Cancelled", "user shell cancelled before execution"));
            }
            let files = OutputFiles::create(&artifact_dir)?;
            let args = shell_arguments(&request.command, request.shell == "powershell");
            let (child, tree) =
                eden_process::spawn(&executable, &args, Path::new(&request.cwd)).await?;
            collect(child, tree, files, None, cancellation, Some(cx)).await
        }
        .await;
        let mut result = execution.unwrap_or_else(super::failed);
        annotate(&mut result, &request, false);
        let cleanup = result
            .error
            .as_ref()
            .filter(|e| e.code == "CleanupFailure")
            .cloned();
        capture.retain_result(serde_json::json!(result))?;
        let _ = sender.send(result);
        match cleanup {
            Some(error) => Err(error),
            None => Ok(()),
        }
    })?;
    receiver
        .await
        .map_err(|_| fault("ToolFailure", "user shell completion lost"))
}

async fn validate_user_request(request: &ShellRequest) -> Result<(), Fault> {
    if !matches!(request.shell.as_str(), "bash" | "powershell") {
        return Err(fault("InvalidInput", "shell must be bash or powershell"));
    }
    let cwd = Path::new(&request.cwd);
    if !cwd.is_absolute() || !tokio::fs::metadata(cwd).await.is_ok_and(|m| m.is_dir()) {
        return Err(fault(
            "InvalidInput",
            "cwd must be an existing absolute directory",
        ));
    }
    Ok(())
}

fn annotate(result: &mut ToolResult, request: &ShellRequest, overridden: bool) {
    if !result.details.is_object() {
        result.details = serde_json::json!({ "original_details": result.details });
    }
    result.details["execution"] = serde_json::json!(request);
    result.details["hook_override"] = serde_json::json!(overridden);
}

fn shell_arguments(command: &str, powershell: bool) -> Vec<&str> {
    if powershell {
        vec![
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            command,
        ]
    } else {
        vec!["--noprofile", "--norc", "-c", command]
    }
}

pub(super) fn timeout(arguments: &Value) -> Result<Option<Duration>, Fault> {
    arguments
        .get("timeout_seconds")
        .map(|value| {
            value
                .as_f64()
                .filter(|value| value.is_finite() && *value > 0.0)
                .and_then(|value| Duration::try_from_secs_f64(value).ok())
                .filter(|value| {
                    !value.is_zero() && std::time::Instant::now().checked_add(*value).is_some()
                })
                .ok_or_else(|| {
                    fault(
                        "InvalidInput",
                        "timeout_seconds must be a positive finite duration",
                    )
                })
        })
        .transpose()
}

struct OutputFiles {
    stdout: tokio::fs::File,
    stderr: tokio::fs::File,
    paths: [PathBuf; 2],
}
impl OutputFiles {
    fn create(root: &Path) -> Result<Self, Fault> {
        std::fs::create_dir_all(root).map_err(file_error)?;
        let root = std::fs::canonicalize(root).map_err(file_error)?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| fault("FileFailure", error.to_string()))?
            .as_nanos();
        let directory = root.join(format!(
            "shell-{}-{stamp}-{}",
            std::process::id(),
            NEXT_OUTPUT.fetch_add(1, Ordering::Relaxed)
        ));
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let mut builder = builder;
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&directory).map_err(file_error)?;
        let paths = [directory.join("stdout.bin"), directory.join("stderr.bin")];
        let open = |path: &Path| {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options
                .open(path)
                .map(tokio::fs::File::from_std)
                .map_err(file_error)
        };
        Ok(Self {
            stdout: open(&paths[0])?,
            stderr: open(&paths[1])?,
            paths,
        })
    }
}
struct Captured {
    tail: VecDeque<u8>,
    bytes: u64,
}
impl Captured {
    fn preview(&self) -> (String, bool) {
        let bytes: Vec<_> = self.tail.iter().copied().collect();
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        let excess = text.len().saturating_sub(PREVIEW_LIMIT);
        let start = text.ceil_char_boundary(excess);
        text.drain(..start);
        (text, self.bytes > bytes.len() as u64 || start > 0)
    }
}
async fn capture(
    mut pipe: impl AsyncRead + Unpin,
    mut output: tokio::fs::File,
    events: Option<(CallContext, &'static str)>,
) -> Result<Captured, Fault> {
    let mut captured = Captured {
        tail: VecDeque::with_capacity(PREVIEW_LIMIT),
        bytes: 0,
    };
    let mut chunk = [0u8; 8192];
    loop {
        let count = pipe.read(&mut chunk).await.map_err(file_error)?;
        if count == 0 {
            output.flush().await.map_err(file_error)?;
            output.sync_all().await.map_err(file_error)?;
            drop(output);
            return Ok(captured);
        }
        output
            .write_all(&chunk[..count])
            .await
            .map_err(file_error)?;
        // Tokio files may defer the OS write; observe disk errors while the child
        // is still live, rather than waiting for a pipe EOF it may never send.
        output.flush().await.map_err(file_error)?;
        if let Some((cx, stream)) = &events {
            // Closing observation admission must not interrupt output retention
            // while the process owner drains and settles a cancelled command.
            let _ = cx.emit(
                "user_shell_output",
                serde_json::json!(ShellOutput {
                    stream: (*stream).into(),
                    bytes: chunk[..count].to_vec(),
                }),
            );
        }
        captured.bytes += count as u64;
        let remove = (captured.tail.len() + count).saturating_sub(PREVIEW_LIMIT);
        captured.tail.drain(..remove);
        captured.tail.extend(&chunk[..count]);
    }
}

pub(super) async fn run(
    cwd: &Path,
    command: &str,
    executable: &str,
    powershell: bool,
    timeout: Option<Duration>,
    artifact_dir: &Path,
    cancellation: eden_plugin_sdk::Cancellation,
) -> Result<ToolResult, Fault> {
    let files = OutputFiles::create(artifact_dir)?;
    let args = shell_arguments(command, powershell);
    let (child, tree) = eden_process::spawn(executable, &args, cwd).await?;
    collect(child, tree, files, timeout, cancellation, None).await
}

async fn collect(
    mut child: tokio::process::Child,
    tree: eden_process::Tree,
    files: OutputFiles,
    timeout: Option<Duration>,
    cancellation: eden_plugin_sdk::Cancellation,
    events: Option<CallContext>,
) -> Result<ToolResult, Fault> {
    // spawn always pipes both streams. Keep the fallback inside the owner so
    // even an unexpected missing pipe cannot bypass the settlement barrier.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let capture = async {
        let stdout = stdout.ok_or_else(|| fault("ToolFailure", "stdout pipe missing"))?;
        let stderr = stderr.ok_or_else(|| fault("ToolFailure", "stderr pipe missing"))?;
        tokio::try_join!(
            capture(
                stdout,
                files.stdout,
                events.clone().map(|cx| (cx, "stdout"))
            ),
            capture(stderr, files.stderr, events.map(|cx| (cx, "stderr")))
        )
    };
    tokio::pin!(capture);
    let deadline = async {
        match timeout {
            Some(duration) => tokio::time::sleep(duration).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(deadline);
    let mut captured = None;
    let mut reason = None;
    let mut status = loop {
        tokio::select! {
            status = child.wait() => {
                break Some(status);
            },
            _ = cancellation.cancelled() => {
                reason = Some(fault(
                    "Cancelled",
                    "shell command cancelled; process tree stopped",
                ));
                break None;
            },
            _ = &mut deadline => {
                reason = Some(fault(
                    "Timeout",
                    "shell command deadline reached; process tree stopped",
                ));
                break None;
            },
            streams = &mut capture, if captured.is_none() => {
                let failed = streams.is_err();
                captured = Some(streams);
                if failed {
                    break None;
                }
            }
        }
    };
    let stopping = status.as_ref().is_none_or(|status| status.is_err());
    let stopped = if stopping {
        tree.terminate_group()
    } else {
        tree.cleanup_descendants()
    };
    // Settlement is attempted even when signalling failed; no output-capture
    // failure may leave the command running while an error is reported.
    if stopped.is_err() {
        tree.settle().await?;
    }
    if status.is_none() {
        status = Some(child.wait().await);
    }
    let settled = tree.settle().await;
    let streams = match captured {
        Some(streams) => streams,
        None => capture.await,
    };
    stopped?;
    settled?;
    let status = status
        .ok_or_else(|| fault("CleanupFailure", "leader status was not observed"))?
        .map_err(|error| fault("ToolFailure", error.to_string()))?;
    let (stdout, stderr) = streams?;
    let (out_preview, out_truncated) = stdout.preview();
    let (err_preview, err_truncated) = stderr.preview();
    let mut text = out_preview;
    if stderr.bytes > 0 {
        text.push_str("\n[stderr]\n");
        text.push_str(&err_preview);
    }
    let error = reason.or_else(|| {
        (!status.success()).then(|| fault("ShellExit", format!("shell exited with {status}")))
    });
    let artifacts = files
        .paths
        .into_iter()
        .zip([("stdout", stdout.bytes), ("stderr", stderr.bytes)])
        .map(|(path, (name, bytes))| Artifact {
            path: path.to_string_lossy().into_owned(),
            name: name.into(),
            bytes,
            media_type: "application/octet-stream".into(),
        })
        .collect();
    Ok(ToolResult {
        text,
        content: vec![],
        artifacts,
        exit_code: status.code(),
        truncated: out_truncated || err_truncated,
        error,
        details: eden_plugin_sdk::serde_json::json!({
            "stdout_bytes": stdout.bytes,
            "stderr_bytes": stderr.bytes,
            "stdout_truncated": out_truncated,
            "stderr_truncated": err_truncated,
            "output_complete": true,
            "process_tree_settled": true,
            "timeout_seconds": timeout.map(|duration| duration.as_secs_f64()),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eden_plugin_sdk::{Cancellation, serde_json::json};
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "eden-shell-p2-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn deadline_is_opt_in_and_rejects_invalid_values() {
        assert_eq!(timeout(&json!({})).unwrap(), None);
        assert_eq!(
            timeout(&json!({ "timeout_seconds": 0.125 })).unwrap(),
            Some(Duration::from_millis(125))
        );
        for value in [json!(0), json!(-1), json!("1"), json!(null), json!(1e100)] {
            assert!(timeout(&json!({ "timeout_seconds": value })).is_err());
        }
    }
    #[tokio::test]
    async fn both_stream_tails_and_original_bytes_survive_without_reexecution() {
        let f = Fixture::new();
        let out = [vec![b'o'; 90_000], b"\nstdout-tail\n".to_vec()].concat();
        let err = [vec![b'e'; 80_000], b"\nstderr-tail\n".to_vec()].concat();
        std::fs::write(f.0.join("source-out"), &out).unwrap();
        std::fs::write(f.0.join("source-err"), &err).unwrap();
        let result = run(
            &f.0,
            "printf x >> executions; cat source-out; cat source-err >&2",
            "bash",
            false,
            None,
            &f.0.join("artifacts"),
            Cancellation::default(),
        )
        .await
        .unwrap();
        assert!(result.text.contains("stdout-tail"), "stdout tail absent");
        assert!(result.text.contains("stderr-tail"), "stderr tail absent");
        assert!(result.text.len() <= super::super::OUTPUT_LIMIT);
        assert!(result.truncated);
        assert_eq!(result.artifacts.len(), 2);
        for (artifact, expected) in result.artifacts.iter().zip([out, err]) {
            assert_eq!(std::fs::read(&artifact.path).unwrap(), expected);
            assert_eq!(artifact.bytes, expected.len() as u64);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&artifact.path)
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
                assert_eq!(
                    std::fs::metadata(Path::new(&artifact.path).parent().unwrap())
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o700
                );
            }
        }
        assert_eq!(std::fs::read(f.0.join("executions")).unwrap(), b"x");
    }
    #[tokio::test]
    async fn timeout_returns_only_after_ready_descendant_releases_connection() {
        use tokio::io::AsyncReadExt;
        let f = Fixture::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let command = format!("exec 3<>/dev/tcp/127.0.0.1/{port}; sleep 300 & printf R >&3; wait");
        let cwd = f.0.clone();
        let task = tokio::spawn(async move {
            run(
                &cwd,
                &command,
                "bash",
                false,
                Some(Duration::from_secs(2)),
                &cwd.join("artifacts"),
                Cancellation::default(),
            )
            .await
        });
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut ready = [0; 1];
        socket.read_exact(&mut ready).await.unwrap();
        assert_eq!(ready, *b"R");
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result.error.unwrap().code, "Timeout");
        match tokio::time::timeout(Duration::from_secs(2), socket.read(&mut ready))
            .await
            .unwrap()
        {
            Ok(0) => {}
            #[cfg(windows)]
            Err(error) if error.raw_os_error() == Some(10054) => {}
            other => panic!("descendant retained connection: {other:?}"),
        }
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn capture_disk_failure_stops_ready_process_tree_before_returning() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let f = Fixture::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let command = format!(
            "exec 3<>/dev/tcp/127.0.0.1/{port}; sleep 300 & printf R >&3; read -r gate <&3; \
             printf output; wait"
        );
        let mut files = OutputFiles::create(&f.0.join("artifacts")).unwrap();
        files.stdout = tokio::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .await
            .unwrap();
        let (child, tree) =
            eden_process::spawn("bash", &["--noprofile", "--norc", "-c", &command], &f.0)
                .await
                .unwrap();
        let task = tokio::spawn(collect(
            child,
            tree,
            files,
            None,
            Cancellation::default(),
            None,
        ));
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut ready = [0; 1];
        socket.read_exact(&mut ready).await.unwrap();
        assert_eq!(ready, *b"R");
        socket.write_all(b"G\n").await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, "FileFailure");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), socket.read(&mut ready))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
}
