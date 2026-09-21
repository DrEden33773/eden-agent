//! The private stdin bridge must finish at a line boundary, without waiting for EOF.
use std::{
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn private_input_reads_one_line_without_eof() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_eden"))
        .args(["auth", "read-input"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"private-code\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"private-code\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn private_input_rejects_oversized_input_without_echoing_it() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_eden"))
        .args(["auth", "read-input"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&vec![b'x'; 65537])
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn login_accepts_only_supported_method_names() {
    use clap::Parser;
    use eden_cli::cli::{AuthAction, Cli, Family};
    let cli = Cli::try_parse_from([
        "eden",
        "auth",
        "login",
        "openai-codex",
        "--method",
        "browser",
    ])
    .unwrap();
    assert!(
        matches!(cli.family, Some(Family::Auth {action: AuthAction::Login {provider, method: Some(method)}}) if provider == "openai-codex" && method == "browser")
    );
    assert!(
        Cli::try_parse_from([
            "eden",
            "auth",
            "login",
            "openai-codex",
            "--method",
            "invalid"
        ])
        .is_err()
    );
    assert!(Cli::try_parse_from(["eden", "auth", "refresh", "openai-codex"]).is_ok());
}
