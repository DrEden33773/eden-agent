//! Tests are outside the doc comment standard; see docs/development-checks.md#doc-comments.
#![allow(missing_docs)]

use std::process::Command;
#[test]
fn independent_history_reads_prefix_without_plugins_or_original_cwd() {
    let dir = std::env::temp_dir().join(format!("eden-history-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("legacy.jsonl");
    let text = concat!(
        "{\"schema_version\":1,\"session_id\":9,\"sequence\":1,\"run_id\":0,\"kind\":\"session\",\"payload\":{\"cwd\":\"/missing-original-project\"}}\n",
        "{\"partial\":",
    );
    let bytes = text.as_bytes();
    std::fs::write(&path, bytes).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_eden"))
        .args(["history", "inspect"])
        .arg(&path)
        .current_dir(&dir)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["session_id"], 9);
    assert!(String::from_utf8_lossy(&output.stderr).contains("incomplete"));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    std::fs::remove_dir_all(dir).unwrap();
}
