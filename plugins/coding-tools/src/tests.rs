use super::*;
use eden_plugin_sdk::serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

struct Project(PathBuf);
impl Project {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "eden-tools-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn request(&self, name: &str, arguments: Value) -> ToolRequest {
        ToolRequest {
            cwd: self.0.to_str().unwrap().into(),
            call_id: "test".into(),
            name: name.into(),
            arguments,
        }
    }
    async fn run(&self, name: &str, arguments: Value) -> ToolResult {
        execute(
            self.request(name, arguments),
            Scope::default(),
            "bash".into(),
        )
        .await
        .unwrap()
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_uses_native_paths_and_explicit_cwd() {
    let project = Project::new();
    std::fs::create_dir(project.0.join("space dir")).unwrap();
    let result = execute(
        project.request(
            "powershell",
            json!({
"command":"[IO.File]::WriteAllText((Join-Path (Get-Location) 'space dir/value.txt'), \
                'native UTF-8'); Get-Content -LiteralPath 'space dir/value.txt'"}),
        ),
        Scope::default(),
        "pwsh".into(),
    )
    .await
    .unwrap();
    assert!(result.error.is_none(), "{result:?}");
    assert!(result.text.contains("native UTF-8"));
    assert_eq!(
        std::fs::read_to_string(project.0.join("space dir/value.txt")).unwrap(),
        "native UTF-8"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_cancellation_reaps_native_descendant_before_completion() {
    let project = Project::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    std::fs::write(
        project.0.join("child.ps1"),
        format!(
            "$client=[Net.Sockets.TcpClient]::new('127.0.0.1',{port}); \
                $stream=$client.GetStream(); $stream.WriteByte(82); $stream.Flush(); \
                Start-Sleep -Seconds 300"
        ),
    )
    .unwrap();
    let request = project.request(
        "powershell",
        json!({"command":"$child=Start-Process pwsh -ArgumentList \
                '-NoProfile','-NonInteractive','-File','child.ps1' -PassThru; Wait-Process \
                -Id $child.Id"}),
    );
    let scope = Scope::default();
    let cancel = scope.cancellation();
    let task = tokio::spawn(execute(request, scope, "pwsh".into()));
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(15), listener.accept())
        .await
        .unwrap()
        .unwrap();
    use tokio::io::AsyncReadExt;
    let mut byte = [0; 1];
    socket.read_exact(&mut byte).await.unwrap();
    assert_eq!(byte, *b"R");
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.error.unwrap().code, "Cancelled");
    assert_closed(
        tokio::time::timeout(Duration::from_secs(5), socket.read(&mut byte))
            .await
            .unwrap(),
    );
}

#[tokio::test]
async fn write_edit_and_read_use_explicit_cwd_and_utf8_line_ranges() {
    let project = Project::new();
    assert!(
        project
            .run(
                "write",
                json!({"path":"nested/example.txt", "content":"一\nsecond\n三\n"})
            )
            .await
            .error
            .is_none()
    );
    assert!(
        project
            .run(
                "edit",
                json!({
                    "path": "nested/example.txt",
                    "old_text": "second",
                    "new_text": "二"
                })
            )
            .await
            .error
            .is_none()
    );
    let result = project
        .run(
            "read",
            json!({"path":"nested/example.txt", "offset":2,"limit":1}),
        )
        .await;
    assert_eq!(result.text, "二\n");
    assert!(!result.truncated);
    assert_eq!(
        std::fs::read_to_string(project.0.join("nested/example.txt")).unwrap(),
        "一\n二\n三\n"
    );
}

#[tokio::test]
async fn exact_edit_rejects_zero_multiple_and_empty_matches_without_writing() {
    let project = Project::new();
    std::fs::write(project.0.join("file"), "aba aba").unwrap();
    for old in ["missing", "aba", ""] {
        let result = project
            .run(
                "edit",
                json!({"path":"file", "old_text":old, "new_text":"lost"}),
            )
            .await;
        assert!(result.error.is_some(), "old={old}");
        assert_eq!(
            std::fs::read_to_string(project.0.join("file")).unwrap(),
            "aba aba"
        );
    }
}

#[tokio::test]
async fn invalid_encoding_and_ranges_are_explicit_errors() {
    let project = Project::new();
    std::fs::write(project.0.join("binary"), [0xff, 0xfe]).unwrap();
    assert!(
        project
            .run("read", json!({"path":"binary"}))
            .await
            .error
            .is_some()
    );
    std::fs::write(project.0.join("text"), "one\ntwo").unwrap();
    for range in [
        json!({"offset":0}),
        json!({"offset":8}),
        json!({"limit":0}),
        json!({"offset":-1}),
    ] {
        let mut args = range;
        args["path"] = json!("text");
        assert!(project.run("read", args).await.error.is_some());
    }
}

#[tokio::test]
async fn long_read_and_shell_output_are_bounded_with_explicit_truncation() {
    let project = Project::new();
    std::fs::write(project.0.join("long"), "界".repeat(50_000)).unwrap();
    let read = project.run("read", json!({"path":"long"})).await;
    assert!(read.truncated);
    assert!(read.text.len() <= 65_536);
    let shell = project
        .run("bash", json!({"command":"printf '%100000s' x"}))
        .await;
    assert!(shell.truncated, "{shell:?}");
    assert!(shell.text.len() <= 65_536);
    assert_eq!(shell.exit_code, Some(0));
}

#[tokio::test]
async fn shell_reports_nonzero_exit_and_cwd_and_missing_shell() {
    let project = Project::new();
    std::fs::write(project.0.join("marker"), "cwd-right").unwrap();
    let result = project
        .run(
            "bash",
            json!({"command":"cat marker; printf error >&2; exit 7"}),
        )
        .await;
    assert_eq!(result.exit_code, Some(7));
    assert!(result.text.contains("cwd-right"));
    assert!(result.text.contains("error"));
    assert!(result.error.is_some());
    let missing = execute(
        project.request("bash", json!({"command":"echo bad"})),
        Scope::default(),
        project.0.join("absent-shell").to_str().unwrap().into(),
    )
    .await
    .unwrap();
    assert_eq!(missing.error.unwrap().code, "ShellUnavailable");
}

fn assert_closed(result: std::io::Result<usize>) {
    match result {
        Ok(0) => {}
        // TerminateJobObject abruptly stops the peer: Winsock reports the
        // observed WSAECONNRESET (10054), not necessarily graceful TCP EOF.
        // Data, timeout, and every unrelated socket error still fail.
        #[cfg(windows)]
        Err(error) if error.raw_os_error() == Some(10054) => {}
        other => panic!("owned connection did not close: {other:?}"),
    }
}

/// Prove the owned connection is closed using a nonblocking OS read.
///
/// `into_std` preserves the fd's nonblocking flag, so a single read can report
/// `WouldBlock` merely because the peer's FIN has not arrived yet. `WouldBlock`
/// is therefore retried until the deadline; any other outcome is judged once.
/// A premature tool result would keep the connection open for the full deadline
/// and still fail, so this cannot mask one.
#[cfg(unix)]
fn assert_closed_now(mut socket: std::net::TcpStream) {
    use std::io::{ErrorKind, Read};
    socket
        .set_nonblocking(true)
        .expect("set socket nonblocking");
    let mut byte = [0u8; 1];
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match socket.read(&mut byte) {
            Ok(0) => return,
            Ok(read) => panic!("owned connection still delivered {read} byte(s)"),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "owned connection did not close before the deadline"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => panic!("owned connection did not close: {error:?}"),
        }
    }
}

// The shell and its background child inherit the socket; closure proves resource
// release. Process settlement is enforced by the group/job exit barriers.
#[tokio::test]
async fn cancellation_stops_shell_descendants_before_completion() {
    let project = Project::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let command = format!("exec 3<>/dev/tcp/127.0.0.1/{port}; sleep 300 & printf R >&3; wait");
    let scope = Scope::default();
    let cancel = scope.cancellation();
    let request = project.request("bash", json!({"command":command}));
    let running = tokio::spawn(execute(request, scope, "bash".into()));
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .unwrap()
        .unwrap();
    // Readiness emitted after the descendant starts, not merely after parent spawn.
    use tokio::io::AsyncReadExt;
    let mut ready = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(5), socket.read_exact(&mut ready))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ready, *b"R");
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.error.unwrap().code, "Cancelled");
    let mut byte = [0u8; 1];
    assert_closed(
        tokio::time::timeout(Duration::from_secs(5), socket.read(&mut byte))
            .await
            .unwrap(),
    );
}

#[tokio::test]
async fn normal_shell_exit_also_cleans_background_descendants() {
    let project = Project::new();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        project.run("bash", json!({"command":"sleep 300 & echo ready; exit 0"})),
    )
    .await
    .unwrap();
    assert_eq!(result.exit_code, Some(0));
    assert!(result.text.contains("ready"));
}

#[tokio::test]
async fn dropping_root_future_keeps_process_cleanup_owned_by_scope() {
    let project = Project::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let command = format!("exec 3<>/dev/tcp/127.0.0.1/{port}; sleep 300 & printf R >&3; wait");
    let scope = Scope::default();
    let cancellation = scope.cancellation();
    let request = project.request("bash", json!({"command":command}));
    let running = tokio::spawn(execute(request, scope, "bash".into()));
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .unwrap()
        .unwrap();
    use tokio::io::AsyncReadExt;
    let mut byte = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(5), socket.read_exact(&mut byte))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(byte, *b"R");
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    cancellation.cancel();
    assert_closed(
        tokio::time::timeout(Duration::from_secs(5), socket.read(&mut byte))
            .await
            .unwrap(),
    );
}

#[tokio::test]
async fn invalid_cwd_is_rejected_before_file_side_effects() {
    let project = Project::new();
    let mut request = project.request(
        "write",
        json!({"path":project.0.join("absent"),"content":"wrong"}),
    );
    request.cwd = ".".into();
    assert_eq!(
        execute(request, Scope::default(), "bash".into())
            .await
            .unwrap()
            .error
            .unwrap()
            .code,
        "InvalidInput"
    );
    assert!(!project.0.join("absent").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_descendant_with_closed_stdio_has_exited_before_result() {
    let project = Project::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let command = format!(
        "(exec 1>&- 2>&-; exec 3<>/dev/tcp/127.0.0.1/{port}; printf R >&3; exec \
                sleep 300) & wait"
    );
    let scope = Scope::default();
    let cancellation = scope.cancellation();
    let task = tokio::spawn(execute(
        project.request("bash", json!({"command":command})),
        scope,
        "bash".into(),
    ));
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .unwrap()
        .unwrap();
    use tokio::io::AsyncReadExt;
    let mut byte = [0; 1];
    tokio::time::timeout(Duration::from_secs(5), socket.read_exact(&mut byte))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(byte, *b"R");
    cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.error.unwrap().code, "Cancelled");
    assert_closed_now(socket.into_std().unwrap());
}

#[cfg(unix)]
#[tokio::test]
async fn foreground_exit_waits_for_descendant_that_closed_stdio() {
    let project = Project::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let command = format!(
        "mkfifo ready; (exec 1>&- 2>&-; exec 3<>/dev/tcp/127.0.0.1/{port}; printf R >&3; echo \
         ready >ready; exec sleep 300) & read line <ready; exit 0"
    );
    let task = tokio::spawn(execute(
        project.request("bash", json!({"command":command})),
        Scope::default(),
        "bash".into(),
    ));
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .unwrap()
        .unwrap();
    use tokio::io::AsyncReadExt;
    let mut byte = [0; 1];
    tokio::time::timeout(Duration::from_secs(5), socket.read_exact(&mut byte))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(byte, *b"R");
    let result = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.exit_code, Some(0));
    assert!(result.error.is_none(), "{result:?}");
    assert_closed_now(socket.into_std().unwrap());
}
