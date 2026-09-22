//! Process boundaries for piped prompt input and command-owned stdin.
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
};
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "eden-prompt-input-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_eden"));
        command
            .current_dir(&self.0)
            .args(["--no-session", "--composition", "missing.json"]);
        command
    }
    fn pipe(&self, args: &[&str], input: &[u8]) -> Output {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn stdin_only_and_multiple_prompts_reach_composition_loading() {
    let scratch = Scratch::new();
    for (args, input) in [
        (&[][..], &b"  piped\n"[..]),
        (&["first", "second"][..], &b""[..]),
        (&[][..], &b" \n\t"[..]),
    ] {
        let output = scratch.pipe(args, input);
        assert_eq!(output.status.code(), Some(1));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("FileFailure (composition)"), "{error}");
        assert!(!scratch.0.join(".eden").exists());
    }
}
#[test]
fn empty_pipe_and_invalid_utf8_fail_before_composition_or_history() {
    let scratch = Scratch::new();
    for (input, expected) in [(&b""[..], "provide a prompt"), (&b"\xff"[..], "stdin")] {
        let output = scratch.pipe(&[], input);
        assert_eq!(output.status.code(), Some(1));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains("composition"), "{error}");
        assert!(output.stdout.is_empty());
        assert!(!scratch.0.join(".eden").exists());
    }
}
#[cfg(unix)]
#[test]
fn stdin_read_failure_is_reported_before_loading_plugins() {
    let scratch = Scratch::new();
    let output = scratch
        .command()
        .arg("prompt")
        .stdin(Stdio::from(fs::File::open(&scratch.0).unwrap()))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("stdin"), "{error}");
    assert!(!error.contains("composition"), "{error}");
}
