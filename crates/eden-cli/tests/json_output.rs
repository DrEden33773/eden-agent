//! Tests are outside the doc comment standard; see docs/development-checks.md#doc-comments.
#![allow(missing_docs)]
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_eden"))
        .args(args)
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .env("TERM", "xterm")
        .output()
        .unwrap()
}

#[test]
fn ordinary_results_indent_and_machine_mode_overrides_forced_color() {
    let path = format!("{}/tests/fixtures/legacy-1.txt", env!("CARGO_MANIFEST_DIR"));
    let plain = run(&["session", "info", &path]);
    assert_eq!(plain.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&plain.stdout).unwrap();
    assert_eq!(
        String::from_utf8(plain.stdout.clone()).unwrap(),
        format!("{}\n", serde_json::to_string_pretty(&value).unwrap())
    );
    for args in [
        vec!["--color", "never"],
        vec!["--json"],
        vec!["--json", "--color", "always"],
    ] {
        let mut command = args.clone();
        command.extend(["session", "info", &path]);
        let output = run(&command);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
            value
        );
        if args.contains(&"--json") {
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                format!("{}\n", serde_json::to_string(&value).unwrap())
            );
        } else {
            assert_eq!(output.stdout, plain.stdout);
        }
    }
    let colored = run(&["--color", "always", "session", "info", &path]);
    assert!(colored.stdout.contains(&27));
    assert!(String::from_utf8_lossy(&colored.stderr).contains("incomplete"));
}

#[test]
fn history_and_tree_keep_uncolored_record_boundaries() {
    let path = format!("{}/tests/fixtures/legacy-1.txt", env!("CARGO_MANIFEST_DIR"));
    for args in [
        vec!["history", "inspect", &path],
        vec!["session", "tree", &path],
    ] {
        let mut command = vec!["--color", "always"];
        command.extend(args);
        let output = run(&command);
        assert_eq!(output.status.code(), Some(2));
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(!text.contains('\x1b'));
        serde_json::from_str::<serde_json::Value>(&text).unwrap();
    }
}
