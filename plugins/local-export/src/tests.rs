use super::*;
use eden_plugin_sdk::protocol::coding::Record;
use serde_json::json;
fn record(sequence: u64, kind: &str, payload: serde_json::Value) -> Record {
    Record {
        schema_version: 2,
        session_id: 1,
        sequence,
        parent_id: (sequence > 1).then_some(sequence - 1),
        branch: "main".into(),
        run_id: 1,
        kind: kind.into(),
        payload,
    }
}
#[test]
fn default_projection_drops_private_state_and_unselected_branch_bytes() {
    let mut records = vec![
        record(1, "composition_lock", json!({ "secret": "CONFIG_CANARY" })),
        record(
            2,
            "message",
            json!({
                "type": "message",
                "role": "user",
                "content": [
                    { "type": "text", "text": "hello </script><script>alert(1)</script>" },
                    { "type": "image", "media_type": "image/png", "data": "IMAGE_CANARY" }
                ],
            }),
        ),
        record(
            3,
            "provider_state",
            json!({
                "type": "provider_state",
                "provider": "p",
                "value": { "secret": "STATE_CANARY" },
            }),
        ),
        record(
            4,
            "message",
            json!({
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "text", "text": "BRANCH_CANARY" }],
            }),
        ),
    ];
    records.push(Record {
        parent_id: Some(2),
        branch: "other".into(),
        ..record(
            5,
            "message",
            json!({
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "text", "text": "chosen" }],
            }),
        )
    });
    let artifact = export(ExportRequest {
        records,
        selection: Selection::default(),
        format: Format::Html,
    })
    .unwrap();
    for canary in [
        "CONFIG_CANARY",
        "STATE_CANARY",
        "IMAGE_CANARY",
        "BRANCH_CANARY",
        "<script>",
    ] {
        assert!(!artifact.content.contains(canary), "{canary}");
    }
    assert!(artifact.content.contains("chosen"));
    assert!(artifact.content.contains("&lt;/script&gt;"));
}
#[test]
fn filtered_jsonl_is_reading_data_and_never_a_restore_backup() {
    let records = vec![record(
        1,
        "custom.extension",
        json!({ "secret": "OPAQUE_CANARY" }),
    )];
    let artifact = export(ExportRequest {
        records,
        selection: Selection::default(),
        format: Format::Jsonl,
    })
    .unwrap();
    assert!(artifact.content.contains("eden-reading-v1"));
    assert!(!artifact.content.contains("OPAQUE_CANARY"));
    assert!(artifact.content.contains("custom.extension"));
}
#[test]
fn explicit_output_inclusion_reports_missing_file_and_removes_path() {
    let records = vec![record(
        1,
        "tool_result",
        json!({
            "type": "tool_result",
            "call_id": "c",
            "result": {
                "text": "preview",
                "exit_code": 0,
                "truncated": true,
                "error": null,
                "artifacts": [{
                    "path": "/MISSING_PATH_CANARY",
                    "name": "stdout",
                    "bytes": 42,
                    "media_type": "text/plain",
                }],
            },
        }),
    )];
    let selection = Selection {
        full_outputs: true,
        ..Selection::default()
    };
    let artifact = export(ExportRequest {
        records,
        selection,
        format: Format::Jsonl,
    })
    .unwrap();
    assert!(!artifact.content.contains("MISSING_PATH_CANARY"));
    assert_eq!(artifact.warnings.len(), 1);
}
#[test]
fn selected_thinking_extracts_display_text_inside_provider_envelope() {
    let records = vec![record(
        1,
        "provider_state",
        json!({
            "type": "provider_state",
            "provider": "anthropic-messages",
            "value": {
                "thinking": "high",
                "target": { "model": "private-target" },
                "state": {
                    "type": "thinking",
                    "thinking": "visible reasoning",
                    "signature": "SIGNATURE_CANARY",
                },
            },
        }),
    )];
    let artifact = export(ExportRequest {
        records,
        selection: Selection {
            thinking: true,
            ..Selection::default()
        },
        format: Format::Html,
    })
    .unwrap();
    assert!(artifact.content.contains("visible reasoning"));
    assert!(!artifact.content.contains("SIGNATURE_CANARY"));
    assert!(!artifact.content.contains("private-target"));
}
