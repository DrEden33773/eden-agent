//! RPC framing and lifecycle through the installed command entry point.
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
};

#[test]
fn rpc_help_declares_a_dedicated_stdio_transport() {
    let output = Command::new(env!("CARGO_BIN_EXE_eden"))
        .args(["rpc", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("JSONL"));
}

struct Client {
    child: Child,
    input: Option<ChildStdin>,
    output: std::sync::mpsc::Receiver<Value>,
    session: u64,
    scratch: std::path::PathBuf,
}
impl Client {
    fn open() -> Self {
        let composition = std::env::var_os("EDEN_RPC_TEST_COMPOSITION")
            .expect("set EDEN_RPC_TEST_COMPOSITION to an installed controlled composition");
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let scratch = std::env::temp_dir().join(format!(
            "eden-rpc-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&scratch).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_eden"))
            .arg("--cwd")
            .arg(&scratch)
            .arg("--global-dir")
            .arg(scratch.join("global"))
            .arg("--composition")
            .arg(composition)
            .arg("rpc")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let (sender, output) = std::sync::mpsc::sync_channel(256);
        std::thread::spawn(move || {
            loop {
                let mut line = String::new();
                if stdout.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let value = serde_json::from_str(&line).unwrap();
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        let mut client = Self {
            child,
            input: Some(input),
            output,
            session: 0,
            scratch,
        };
        let ready = client.next();
        assert_eq!(ready["type"], "ready");
        client.session = ready["session_id"].as_u64().unwrap();
        client
    }
    fn exit(&mut self) -> std::process::ExitStatus {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "RPC process failed to close"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    fn next(&mut self) -> Value {
        self.output
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("RPC response deadline")
    }
    fn request(&mut self, id: &str, method: &str, params: Value) {
        let frame = json!({
            "version": 1,
            "id": id,
            "session_id": self.session,
            "method": method,
            "params": params,
        });
        writeln!(self.input.as_mut().unwrap(), "{frame}").unwrap();
    }
    fn response(&mut self, id: &str) -> Value {
        loop {
            let message = self.next();
            if message["id"] == id {
                return message;
            }
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

#[test]
#[ignore = "requires an installed controlled composition in EDEN_RPC_TEST_COMPOSITION"]
fn malformed_frames_and_version_errors_do_not_consume_following_requests() {
    let mut client = Client::open();
    client
        .input
        .as_mut()
        .unwrap()
        .write_all(b"{broken}\r\n\xff\n{\"version\":2,\"id\":\"version\",\"method\":\"ping\"}\n")
        .unwrap();
    let mut errors = Vec::new();
    while errors.len() < 3 {
        let value = client.next();
        if value["type"] == "error" {
            errors.push(value);
        }
    }
    assert_eq!(errors[2]["id"], "version");
    let frame = json!({
        "version": 1,
        "id": "split",
        "session_id": client.session,
        "method": "ping",
        "params": { "text": "a\u{2028}b\u{2029}中文" },
    })
    .to_string();
    for byte in frame.bytes() {
        client.input.as_mut().unwrap().write_all(&[byte]).unwrap();
    }
    client.input.as_mut().unwrap().write_all(b"\r\n").unwrap();
    assert_eq!(client.response("split")["type"], "result");
    client.request("shutdown", "shutdown", json!({}));
    assert_eq!(client.response("shutdown")["type"], "result");
    assert!(client.exit().success());
}

#[test]
#[ignore = "requires an installed controlled composition in EDEN_RPC_TEST_COMPOSITION"]
fn prompt_acceptance_precedes_settled_and_stale_session_is_rejected() {
    let mut client = Client::open();
    client.request("prompt", "prompt", json!({ "text": "protocol test" }));
    let accepted = client.response("prompt");
    assert_eq!(accepted["type"], "accepted");
    loop {
        let event = client.next();
        if event["type"] == "event" && event["event"]["kind"] == "settled" {
            assert_eq!(event["event"]["run_id"], accepted["run_id"]);
            break;
        }
    }
    let stale = json!({
        "version": 1,
        "id": "stale",
        "session_id": client.session + 1,
        "method": "prompt",
        "params": { "text": "never run" },
    });
    writeln!(client.input.as_mut().unwrap(), "{stale}").unwrap();
    assert_eq!(client.response("stale")["error"]["code"], "SessionMismatch");
    client.request("shutdown", "shutdown", json!({}));
    assert_eq!(client.response("shutdown")["type"], "result");
    assert!(client.exit().success());
}

#[test]
#[ignore = "requires an installed controlled composition in EDEN_RPC_TEST_COMPOSITION"]
fn session_replacement_changes_identity_after_old_session_closes() {
    let mut client = Client::open();
    let old = client.session;
    client.request("replace", "session.new", json!({}));
    let replaced = client.response("replace");
    assert_eq!(replaced["type"], "result");
    client.session = replaced["session_id"].as_u64().unwrap();
    assert_ne!(client.session, old);
    let stale = json!({ "version": 1, "id": "stale", "session_id": old, "method": "state" });
    writeln!(client.input.as_mut().unwrap(), "{stale}").unwrap();
    assert_eq!(client.response("stale")["error"]["code"], "SessionMismatch");
    client.request("shutdown", "shutdown", json!({}));
    assert_eq!(client.response("shutdown")["type"], "result");
    assert!(client.exit().success());
}

#[test]
#[ignore = "requires an installed controlled composition in EDEN_RPC_TEST_COMPOSITION"]
fn eof_accepts_an_unterminated_final_frame_then_closes() {
    let mut client = Client::open();
    let frame = json!({
        "version": 1,
        "id": "tail",
        "session_id": client.session,
        "method": "ping",
    });
    write!(client.input.as_mut().unwrap(), "{frame}").unwrap();
    client.input.take();
    assert_eq!(client.response("tail")["type"], "result");
    assert!(client.exit().success());
}

#[test]
#[ignore = "requires an installed controlled composition in EDEN_RPC_TEST_COMPOSITION"]
fn closed_stdout_terminates_even_with_stdin_still_open() {
    let mut client = Client::open();
    let (_sender, replacement) = std::sync::mpsc::channel();
    drop(std::mem::replace(&mut client.output, replacement));
    client.request(
        "prompt",
        "prompt",
        json!({ "text": "large output ".repeat(10000) }),
    );
    assert!(!client.exit().success());
}

#[test]
#[ignore = "requires an installed controlled composition in EDEN_RPC_TEST_COMPOSITION"]
fn oversized_frame_closes_without_waiting_for_stdin_eof() {
    let mut client = Client::open();
    let _ = client
        .input
        .as_mut()
        .unwrap()
        .write_all(&vec![b'x'; 1024 * 1024 + 1]);
    loop {
        let message = client.next();
        if message["type"] == "error" {
            assert_eq!(message["error"]["code"], "FrameTooLarge");
            break;
        }
    }
    assert!(!client.exit().success());
}

#[cfg(unix)]
#[test]
#[ignore = "requires an installed controlled composition in EDEN_RPC_TEST_COMPOSITION"]
fn termination_signal_closes_while_stdin_remains_open() {
    let mut client = Client::open();
    assert!(
        Command::new("kill")
            .args(["-TERM", &client.child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(client.exit().code(), Some(143));
}

#[cfg(unix)]
fn start_shell(client: &mut Client) -> (u64, u32) {
    let command = r"printf 'rpc-shell-ready:%s\n' $$; sleep 60";
    client.request(
        "shell",
        "shell",
        json!({ "command": command, "shell": "bash", "exclude_from_context": true }),
    );
    let accepted = client.response("shell");
    assert_eq!(accepted["type"], "accepted");
    let run = accepted["run_id"].as_u64().unwrap();
    let mut text = String::new();
    loop {
        let event = client.next();
        if event["event"]["kind"] == "user_shell_output" {
            let bytes: Vec<u8> =
                serde_json::from_value(event["event"]["payload"]["bytes"].clone()).unwrap();
            text.push_str(&String::from_utf8(bytes).unwrap());
            if let Some(line) = text
                .lines()
                .find(|line| line.starts_with("rpc-shell-ready:"))
            {
                let pid = line.trim_start_matches("rpc-shell-ready:").parse().unwrap();
                return (run, pid);
            }
        }
    }
}

#[cfg(unix)]
fn assert_process_gone(pid: u32) {
    assert!(
        !Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
}

#[cfg(unix)]
#[test]
#[ignore = "requires controlled composition with user-shell; run without signal isolation"]
fn shell_cancel_is_read_while_running_and_settled_is_a_process_barrier() {
    let mut client = Client::open();
    let (run, pid) = start_shell(&mut client);
    client.request("cancel", "shell.cancel", json!({ "run_id": run }));
    assert_eq!(client.response("cancel")["type"], "result");
    loop {
        let value = client.next();
        if value["event"]["kind"] == "settled" && value["event"]["run_id"] == run {
            assert_eq!(value["event"]["payload"]["outcome"]["status"], "cancelled");
            break;
        }
    }
    assert_process_gone(pid);
    client.request("shutdown", "shutdown", json!({}));
    assert_eq!(client.response("shutdown")["type"], "result");
    assert!(client.exit().success());
}

#[cfg(unix)]
#[test]
#[ignore = "requires controlled composition with user-shell; run without signal isolation"]
fn eof_cancels_active_shell_before_process_exit() {
    let mut client = Client::open();
    let (_run, pid) = start_shell(&mut client);
    client.input.take();
    assert!(client.exit().success());
    assert_process_gone(pid);
}
