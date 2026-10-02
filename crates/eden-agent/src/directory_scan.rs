//! Catalogs use the same strict consumer scanner as binding preflight and immutable readers.
use eden_protocol::{
    Fault,
    history::HistoryScan,
    history_read::{ReadView, read_view},
};
use std::fs::File;
pub(crate) fn inspect(
    file: File,
    cancel: &eden_plugin_sdk::Cancellation,
) -> Result<Option<HistoryScan>, Fault> {
    read_view(file, ReadView::Summary, &|| cancel.is_cancelled())
}
#[cfg(test)]
mod tests {
    use super::*;
    use eden_protocol::coding::Record;
    use serde_json::Value;
    use serde_json::json;
    fn record(sequence: u64, kind: &str, payload: Value) -> Record {
        Record {
            schema_version: 2,
            session_id: 17,
            sequence,
            run_id: 0,
            parent_id: sequence.checked_sub(1).filter(|parent| *parent > 0),
            branch: "main".into(),
            kind: kind.into(),
            payload,
        }
    }
    fn scanned(bytes: &[u8]) -> (std::path::PathBuf, HistoryScan) {
        let path = std::env::temp_dir().join(format!(
            "eden-directory-scan-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, bytes).unwrap();
        let scan = inspect(
            File::open(&path).unwrap(),
            &eden_plugin_sdk::Cancellation::default(),
        )
        .unwrap()
        .unwrap();
        (path, scan)
    }
    #[test]
    fn summary_matches_the_complete_reader_without_allocating_cumulative_audit_payloads() {
        let records = vec![
            record(
                1,
                "session",
                json!({ "cwd": "/fixture", "origin": { "session": 7 } }),
            ),
            record(
                2,
                "message",
                json!({
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "text", "text": "中文  first\nline" }],
                }),
            ),
            record(
                3,
                "model_request",
                json!({ "input": { "items": [{ "audit": "x".repeat(8 * 1024 * 1024) }] } }),
            ),
            record(
                4,
                "session_metadata",
                json!({ "name": "Saved name", "tags": ["tag"], "activity_unix": 17 }),
            ),
            record(
                5,
                "model_selection",
                json!({ "selection": { "provider": "fixture", "model": "test" } }),
            ),
        ];
        let bytes = eden_protocol::history::encode_transaction(&records).unwrap();
        let (path, summary) = scanned(&bytes);
        assert!(summary.diagnostic.is_none());
        assert_eq!(summary.records.len(), records.len());
        assert!(serde_json::to_vec(&summary.records).unwrap().len() < 2048);
        let complete = eden_protocol::history::scan_records(&bytes);
        let expected =
            super::super::describe_records(path.clone(), complete.records, complete.diagnostic);
        let actual =
            super::super::describe_records(path.clone(), summary.records, summary.diagnostic);
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn unknown_node_extensions_are_validated_before_the_whole_transaction_is_admitted() {
        let prefix = eden_protocol::history::encode_transaction(&[record(
            1,
            "session",
            json!({ "cwd": "/fixture" }),
        )])
        .unwrap();
        let mut node = serde_json::to_value(record(2, "message", json!({}))).unwrap();
        node["extension"] = json!({ "nested": "extension-text" });
        let tail = json!({
            "schema_version": 2,
            "transaction": [node, record(3, "message", json!({}))],
        })
        .to_string()
            + "\n";
        for (replacement, valid) in [
            ("extension-text", true),
            (r"\uD800", false),
            (r"\uDC00", false),
            (r"\uD800\uD800", false),
        ] {
            let bytes = [
                prefix.as_slice(),
                tail.replace("extension-text", replacement).as_bytes(),
            ]
            .concat();
            let complete = eden_protocol::history::scan_records(&bytes);
            let (path, summary) = scanned(&bytes);
            assert_eq!(complete.records.len(), if valid { 3 } else { 1 });
            assert_eq!(
                summary.records.len(),
                complete.records.len(),
                "{replacement}"
            );
            assert_eq!(summary.diagnostic.is_some(), complete.diagnostic.is_some());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn damaged_ignored_audit_and_tree_transactions_preserve_only_the_validated_prefix() {
        let prefix = eden_protocol::history::encode_transaction(&[record(
            1,
            "session",
            json!({ "cwd": "/fixture" }),
        )])
        .unwrap();
        let good = eden_protocol::history::encode_transaction(&[record(
            2,
            "model_request",
            json!({ "input": "unicode" }),
        )])
        .unwrap();
        let mut broken = record(3, "message", json!({}));
        broken.parent_id = Some(999);
        let bad_tree =
            eden_protocol::history::encode_transaction(&[record(2, "message", json!({})), broken])
                .unwrap();
        for tail in [
            good[..good.len() - 1].to_vec(),
            String::from_utf8(good.clone())
                .unwrap()
                .replace("unicode", "\\uD800")
                .into_bytes(),
            String::from_utf8(good.clone())
                .unwrap()
                .replace("unicode", "\\q")
                .into_bytes(),
            bad_tree,
        ] {
            let mut bytes = prefix.clone();
            bytes.extend(tail);
            bytes.extend(&good);
            let (path, summary) = scanned(&bytes);
            let full = eden_protocol::history::scan_records(&bytes);
            assert!(summary.diagnostic.is_some(), "{summary:?}");
            assert!(full.diagnostic.is_some());
            assert_eq!(summary.records.len(), full.records.len());
            assert_eq!(summary.records.len(), 1);
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            std::fs::remove_file(path).unwrap();
        }
    }
    #[test]
    fn legacy_lines_and_nonobject_payloads_retain_the_same_summary_prefix() {
        let mut bytes = Vec::new();
        for (index, payload) in [
            json!({ "cwd": "/legacy" }),
            json!("extension scalar"),
            json!([1, 2]),
            Value::Null,
        ]
        .into_iter()
        .enumerate()
        {
            let mut record = record(index as u64 + 1, "extension", payload);
            record.schema_version = 1;
            bytes.extend(serde_json::to_vec(&record).unwrap());
            bytes.push(b'\n');
        }
        let (path, summary) = scanned(&bytes);
        let full = eden_protocol::history::scan_records(&bytes);
        assert!(summary.diagnostic.is_none());
        assert_eq!(summary.records.len(), full.records.len());
        assert_eq!(
            summary
                .records
                .iter()
                .map(|record| record.parent_id)
                .collect::<Vec<_>>(),
            full.records
                .iter()
                .map(|record| record.parent_id)
                .collect::<Vec<_>>()
        );
        std::fs::remove_file(path).unwrap();
    }
}
