use super::*;
use eden_plugin_sdk::serde_json::{self, Value, json};
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
                "command": concat!(
                    "[IO.File]::WriteAllText((Join-Path (Get-Location) 'space dir/value.txt'), ",
                    "'native UTF-8'); Get-Content -LiteralPath 'space dir/value.txt'",
                ),
            }),
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
            concat!(
                "$client=[Net.Sockets.TcpClient]::new('127.0.0.1',{port}); $stream=$client.GetStream(); ",
                "$stream.WriteByte(82); $stream.Flush(); Start-Sleep -Seconds 300",
            ),
            port = port,
        ),
    )
    .unwrap();
    let request = project.request(
        "powershell",
        json!({
            "command": concat!(
                           "$child=Start-Process pwsh -ArgumentList '-NoProfile','-NonInteractive','-File','child.ps1' ",
                           "-PassThru; Wait-Process -Id $child.Id",
                       ),
        }),
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
                json!({ "path": "nested/example.txt", "content": "一\nsecond\n三\n" })
            )
            .await
            .error
            .is_none()
    );
    assert!(
        project
            .run(
                "edit",
                json!({ "path": "nested/example.txt", "old_text": "second", "new_text": "二" })
            )
            .await
            .error
            .is_none()
    );
    let result = project
        .run(
            "read",
            json!({ "path": "nested/example.txt", "offset": 2, "limit": 1 }),
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
                json!({ "path": "file", "old_text": old, "new_text": "lost" }),
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
            .run("read", json!({ "path": "binary" }))
            .await
            .error
            .is_some()
    );
    std::fs::write(project.0.join("text"), "one\ntwo").unwrap();
    for range in [
        json!({ "offset": 0 }),
        json!({ "offset": 8 }),
        json!({ "limit": 0 }),
        json!({ "offset": -1 }),
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
    let read = project.run("read", json!({ "path": "long" })).await;
    assert!(read.truncated);
    assert!(read.text.len() <= 65_536);
    let shell = project
        .run("bash", json!({ "command": "printf '%100000s' x" }))
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
            json!({ "command": "cat marker; printf error >&2; exit 7" }),
        )
        .await;
    assert_eq!(result.exit_code, Some(7));
    assert!(result.text.contains("cwd-right"));
    assert!(result.text.contains("error"));
    assert!(result.error.is_some());
    let missing = execute(
        project.request("bash", json!({ "command": "echo bad" })),
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
    let request = project.request("bash", json!({ "command": command }));
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
        project.run(
            "bash",
            json!({ "command": "sleep 300 & echo ready; exit 0" }),
        ),
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
    let request = project.request("bash", json!({ "command": command }));
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
        json!({ "path": project.0.join("absent"), "content": "wrong" }),
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
        concat!(
            "(exec 1>&- 2>&-; exec 3<>/dev/tcp/127.0.0.1/{port}; printf R >&3; exec sleep 300) & ",
            "wait",
        ),
        port = port,
    );
    let scope = Scope::default();
    let cancellation = scope.cancellation();
    let task = tokio::spawn(execute(
        project.request("bash", json!({ "command": command })),
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
        concat!(
            "mkfifo ready; (exec 1>&- 2>&-; exec 3<>/dev/tcp/127.0.0.1/{port}; printf R >&3; echo ",
            "ready >ready; exec sleep 300) & read line <ready; exit 0",
        ),
        port = port,
    );
    let task = tokio::spawn(execute(
        project.request("bash", json!({ "command": command })),
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

#[test]
fn skill_resource_uri_uses_frozen_source_and_enforces_tool_selection() {
    use eden_plugin_sdk::{
        abi::{Bytes, HostApi, Reply},
        protocol::{Outcome, Request, Terminal, resources as r},
        runtime,
    };
    use std::sync::mpsc;

    unsafe extern "C" fn receive(context: usize, bytes: Bytes) {
        // SAFETY: The test retains this sender until the instance is destroyed.
        let sender = unsafe { &*(context as *const mpsc::Sender<Value>) };
        // SAFETY: The runtime borrows the serialized reply for this callback.
        let value = unsafe { bytes.decode::<Value>() }.unwrap();
        sender.send(value).unwrap();
    }
    unsafe extern "C" fn resource(context: usize, bytes: Bytes, reply: Reply) -> u64 {
        // SAFETY: The test retains immutable source data through instance destruction.
        let source = unsafe { &*(context as *const r::ResourceReply) };
        // SAFETY: The SDK owns this request span throughout this synchronous call.
        let request: Request = unsafe { bytes.decode() }.unwrap();
        let result = if request.contract == r::SOURCE {
            let request: r::ResourceRequest = serde_json::from_value(request.payload).unwrap();
            let mut response = source.clone();
            if matches!(request, r::ResourceRequest::Snapshot) {
                response.text = None;
            }
            Terminal {
                outcome: Outcome::Completed(serde_json::to_value(response).unwrap()),
                cleanup_errors: vec![],
            }
        } else {
            Terminal::failed(fault("UnexpectedService", request.contract))
        };
        // SAFETY: The SDK supplied one live completion receiver for this request.
        unsafe { reply.send(&result) };
        1
    }
    unsafe extern "C" fn cancel(_: usize, _: u64) {}
    unsafe extern "C" fn event(_: usize, _: Bytes) {}

    let project = Project::new();
    let disk = project.0.join("SKILL.md");
    std::fs::write(&disk, "CHANGED DISK BODY").unwrap();
    let mut source = r::ResourceReply {
        snapshot: r::Snapshot {
            revision: 7,
            skills: vec![r::Resource {
                name: "frozen".into(),
                description: "read frozen instructions".into(),
                path: disk.display().to_string(),
                model_invocable: true,
            }],
            ..Default::default()
        },
        text: Some("FROZEN SOURCE BODY".into()),
    };
    for (selected, invocable, expected_error) in [
        (true, true, None),
        (true, false, Some("InvalidInput")),
        (false, true, Some("UnknownTool")),
    ] {
        source.snapshot.skills[0].model_invocable = invocable;
        let (sender, receiver) = mpsc::channel::<Value>();
        let reply = Reply {
            context: &sender as *const _ as usize,
            call: receive,
        };
        let host = HostApi {
            context: &source as *const _ as usize,
            request: resource,
            cancel,
            event,
        };
        let config = serde_json::to_vec(&json!({
            "read_only": true,
            "tools": if selected {
                    vec!["read"]
                } else {
                    vec![]
                },
        }))
        .unwrap();
        // SAFETY: Configuration, callbacks and source data remain valid until destruction.
        let instance =
            unsafe { runtime::create(create, descriptor, host, Bytes::new(&config), reply) };
        assert_ne!(instance, 0);
        let request = Request {
            session_id: 1,
            run_id: 1,
            contract: TOOL.into(),
            payload: serde_json::to_value(
                project.request("read", json!({ "path": "eden-resource://skill/frozen" })),
            )
            .unwrap(),
        };
        let bytes = serde_json::to_vec(&request).unwrap();
        // SAFETY: The instance is live and start borrows request bytes synchronously.
        let operation = unsafe { runtime::start(instance, Bytes::new(&bytes), reply) };
        let completed = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        // SAFETY: Completion was received; release precedes exclusive destruction.
        unsafe {
            runtime::release(instance, operation);
            runtime::destroy(instance);
        }
        let terminal: Terminal = serde_json::from_value(completed).unwrap();
        match expected_error {
            Some(code) => assert_eq!(terminal.into_result().unwrap_err().code, code),
            None => {
                let result: ToolResult =
                    serde_json::from_value(terminal.into_result().unwrap()).unwrap();
                assert_eq!(result.text, "FROZEN SOURCE BODY");
            }
        }
    }
}

#[tokio::test]
async fn image_read_returns_self_contained_content() {
    use eden_plugin_sdk::protocol::coding::Block;
    let project = Project::new();
    let bytes = b"\x89PNG\r\n\x1a\nimage payload";
    std::fs::write(project.0.join("picture.dat"), bytes).unwrap();
    let result = project.run("read", json!({ "path": "picture.dat" })).await;
    assert!(result.error.is_none(), "{result:?}");
    assert!(
        matches!(&result.content[..], [Block::Image { media_type, data }] if media_type == "image/png" && !data.is_empty())
    );
}

#[tokio::test]
async fn read_continuation_preserves_complete_lines_and_long_unicode_line() {
    let project = Project::new();
    let original = format!("first\n{}\nlast\n", "界".repeat(30_000));
    std::fs::write(project.0.join("text"), &original).unwrap();
    let first = project
        .run("read", json!({ "path": "text", "limit": 1 }))
        .await;
    assert_eq!(first.text, "first\n");
    assert_eq!(first.details["next_offset"], 2);
    let second = project
        .run("read", json!({ "path": "text", "offset": 2 }))
        .await;
    assert_eq!(second.details["next_offset"], 2);
    assert!(second.details["next_byte_offset"].as_u64().unwrap() > 0);
    let third = project
        .run(
            "read",
            json!({
                "path": "text",
                "offset": second.details["next_offset"],
                "byte_offset": second.details["next_byte_offset"],
            }),
        )
        .await;
    assert_eq!(
        format!("{}{}{}", first.text, second.text, third.text),
        original
    );
    assert_eq!(third.details["complete"], true);
    let invalid = project
        .run(
            "read",
            json!({ "path": "text", "offset": 2, "byte_offset": 1 }),
        )
        .await;
    assert!(invalid.error.is_some());
}

#[tokio::test]
async fn batch_edit_validates_original_and_preserves_bom_crlf() {
    let project = Project::new();
    let original = "\u{feff}one\r\ntwo\r\nthree\r\n";
    std::fs::write(project.0.join("text"), original).unwrap();
    let bad = project
        .run(
            "edit",
            json!({
                "path": "text",
                "edits": [
                    { "old_text": "one\ntwo", "new_text": "changed" },
                    { "old_text": "absent", "new_text": "lost" },
                ],
            }),
        )
        .await;
    assert!(bad.error.is_some());
    assert_eq!(
        std::fs::read_to_string(project.0.join("text")).unwrap(),
        original
    );
    let good = project
        .run(
            "edit",
            json!({
                "path": "text",
                "edits": [
                    { "old_text": "one\ntwo", "new_text": "first\nsecond" },
                    { "old_text": "three", "new_text": "third" },
                ],
            }),
        )
        .await;
    assert!(good.error.is_none(), "{good:?}");
    assert_eq!(
        std::fs::read_to_string(project.0.join("text")).unwrap(),
        "\u{feff}first\r\nsecond\r\nthird\r\n"
    );
    assert_eq!(good.details["edits"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn tolerant_edit_is_explicit_and_refuses_ambiguous_normalized_matches() {
    let project = Project::new();
    let original = "“hello”—world  \n";
    std::fs::write(project.0.join("text"), original).unwrap();
    let args = json!({ "path": "text", "old_text": "\"hello\"-world", "new_text": "done" });
    assert!(project.run("edit", args.clone()).await.error.is_some());
    let mut tolerant = args;
    tolerant["mode"] = json!("tolerant");
    let result = project.run("edit", tolerant.clone()).await;
    assert!(result.error.is_none(), "{result:?}");
    assert_eq!(result.details["mode"], "tolerant");
    assert_eq!(
        std::fs::read_to_string(project.0.join("text")).unwrap(),
        "done\n"
    );
    let ambiguous = "“hello”—world\n\"hello\"-world\n";
    std::fs::write(project.0.join("text"), ambiguous).unwrap();
    assert!(project.run("edit", tolerant).await.error.is_some());
    assert_eq!(
        std::fs::read_to_string(project.0.join("text")).unwrap(),
        ambiguous
    );
}

#[tokio::test]
async fn overlapping_batch_edits_and_overlapping_matches_leave_file_unchanged() {
    let project = Project::new();
    std::fs::write(project.0.join("text"), "abcdef aaa").unwrap();
    for arguments in [
        json!({
            "path": "text",
            "edits": [
                { "old_text": "abc", "new_text": "x" },
                { "old_text": "cde", "new_text": "y" },
            ],
        }),
        json!({ "path": "text", "old_text": "aa", "new_text": "x" }),
    ] {
        assert!(project.run("edit", arguments).await.error.is_some());
        assert_eq!(
            std::fs::read_to_string(project.0.join("text")).unwrap(),
            "abcdef aaa"
        );
    }
}

#[tokio::test]
async fn image_signatures_roundtrip_exact_bytes_and_reject_over_limit() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use eden_plugin_sdk::protocol::coding::Block;
    let project = Project::new();
    for (bytes, media_type) in [
        (b"\xff\xd8\xffJPEG".as_slice(), "image/jpeg"),
        (b"GIF89aGIF".as_slice(), "image/gif"),
        (b"RIFF1234WEBPdata".as_slice(), "image/webp"),
    ] {
        std::fs::write(project.0.join("image"), bytes).unwrap();
        let result = project.run("read", json!({ "path": "image" })).await;
        match &result.content[..] {
            [
                Block::Image {
                    media_type: actual,
                    data,
                },
            ] => {
                assert_eq!(actual, media_type);
                assert_eq!(STANDARD.decode(data).unwrap(), bytes);
            }
            _ => panic!("missing image content: {result:?}"),
        }
    }
    let mut bytes = b"GIF89a".to_vec();
    bytes.resize(10 * 1024 * 1024 + 1, 0);
    std::fs::write(project.0.join("image"), bytes).unwrap();
    assert!(
        project
            .run("read", json!({ "path": "image" }))
            .await
            .error
            .is_some()
    );
}

#[tokio::test]
async fn read_byte_budget_defers_whole_line_and_empty_file_is_complete() {
    let project = Project::new();
    let first_line = format!("{}\n", "a".repeat(40_000));
    let second_line = format!("{}\n", "b".repeat(40_000));
    std::fs::write(project.0.join("text"), format!("{first_line}{second_line}")).unwrap();
    let first = project.run("read", json!({ "path": "text" })).await;
    assert_eq!(first.text, first_line);
    assert_eq!(first.details["next_offset"], 2);
    assert_eq!(first.details["next_byte_offset"], 0);
    assert_eq!(first.details["partial_line"], false);
    std::fs::write(project.0.join("text"), "").unwrap();
    let empty = project.run("read", json!({ "path": "text" })).await;
    assert_eq!(empty.details["complete"], true);
    assert_eq!(empty.text, "");
}

#[cfg(windows)]
#[tokio::test]
async fn bash_path_roundtrip_read_edit_preserves_space_and_unicode_cwd() {
    let project = Project::new();
    let cwd = project.0.join("space 目录");
    std::fs::create_dir(&cwd).unwrap();
    let result = execute_with_artifacts(
        ToolRequest {
            cwd: cwd.to_str().unwrap().into(),
            call_id: "path".into(),
            name: "bash".into(),
            arguments: json!({
                "command": "printf 'before\n' > '文件.txt'; printf '%s/文件.txt\n' \"$PWD\"",
            }),
        },
        Scope::default(),
        "bash".into(),
        project.0.join("artifacts"),
    )
    .await
    .unwrap();
    assert!(result.error.is_none(), "{result:?}");
    let stdout = result
        .artifacts
        .iter()
        .find(|artifact| artifact.name == "stdout")
        .unwrap();
    let path = std::fs::read_to_string(&stdout.path)
        .unwrap()
        .trim()
        .to_owned();
    assert!(
        path.starts_with('/'),
        "expected Bash drive spelling: {path}"
    );
    let read = project.run("read", json!({ "path": path })).await;
    assert!(read.text.contains("before"), "{read:?}");
    let edited = project
        .run(
            "edit",
            json!({ "path": path, "old_text": "before", "new_text": "after" }),
        )
        .await;
    assert!(edited.error.is_none(), "{edited:?}");
    assert_eq!(
        std::fs::read_to_string(cwd.join("文件.txt")).unwrap(),
        "after\n"
    );
}
