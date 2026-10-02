//! Public history validation and active-branch projection, independent of native plugins.

use crate::{
    Fault,
    coding::{Record, StoreView},
};
use serde::{Deserialize, Serialize};

/// A validated committed prefix and, if present, the first damage diagnostic.
/// The original bytes are never changed, and records after damage are never used.
#[derive(Clone, Debug)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct HistoryScan {
    pub records: Vec<Record>,
    pub diagnostic: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Transaction {
    schema_version: u32,
    transaction: Vec<Record>,
}

fn invalid(message: impl Into<String>) -> Fault {
    Fault::new("PersistenceFailure", "public-history", message)
}

/// Validate one next node against an already validated prefix. Readers must publish
/// a transaction only after every node passes; this check does not write history.
pub fn validate_next(records: &[Record], record: &Record) -> Result<(), Fault> {
    if !matches!(record.schema_version, 1 | 2)
        || record.sequence != records.len() as u64 + 1
        || record.branch.trim().is_empty()
        || record.kind.trim().is_empty()
        || records.first().is_some_and(|first| {
            first.session_id != record.session_id || first.schema_version != record.schema_version
        })
    {
        return Err(invalid(
            "unsupported or mixed schema, discontinuous sequence, empty branch/kind, or mixed \
             identity",
        ));
    }
    if let Some(parent) = record.parent_id {
        if parent == 0
            || parent >= record.sequence
            || records[parent as usize - 1].kind == "branch_selected"
        {
            return Err(invalid("parent must reference an earlier tree node"));
        }
    } else if !records.is_empty() {
        return Err(invalid("only the first tree node may have no parent"));
    }
    if record.kind == "branch_selected" {
        let target = record
            .payload
            .get("target")
            .and_then(serde_json::Value::as_u64);
        let branch = record
            .payload
            .get("branch")
            .and_then(serde_json::Value::as_str);
        if target.is_none() || target != record.parent_id || branch != Some(record.branch.as_str())
        {
            return Err(invalid("invalid branch selection"));
        }
    }
    Ok(())
}

/// Validate an expanded public history, including identities and tree links.
pub fn validate_records(records: &[Record]) -> Result<(), Fault> {
    for (index, record) in records.iter().enumerate() {
        validate_next(&records[..index], record)?;
    }
    Ok(())
}

/// Read v1 linear lines and v2 atomic transaction envelopes without executing work.
/// A final line lacking its newline is uncommitted even when its JSON parses.
pub fn scan_records(bytes: &[u8]) -> HistoryScan {
    let mut scan = HistoryScan {
        records: vec![],
        diagnostic: None,
    };
    for (index, line) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
        let start = scan.records.len();
        let result = (|| -> Result<(), Fault> {
            if !line.ends_with(b"\n") {
                return Err(invalid("incomplete history tail; source preserved"));
            }
            let value: serde_json::Value = serde_json::from_slice(line)
                .map_err(|error| invalid(format!("invalid public history JSON: {error}")))?;
            let records = if value.get("transaction").is_some() {
                let transaction: Transaction =
                    serde_json::from_value(value).map_err(|error| invalid(error.to_string()))?;
                if transaction.schema_version != 2
                    || transaction.transaction.is_empty()
                    || transaction
                        .transaction
                        .iter()
                        .any(|record| record.schema_version != 2)
                {
                    return Err(invalid("invalid transaction schema or empty transaction"));
                }
                transaction.transaction
            } else {
                let mut record: Record =
                    serde_json::from_value(value).map_err(|error| invalid(error.to_string()))?;
                if record.schema_version != 1 {
                    return Err(invalid(
                        "v2 records require a committed transaction envelope",
                    ));
                }
                record.parent_id = scan.records.last().map(|previous| previous.sequence);
                record.branch = "main".into();
                vec![record]
            };
            for record in records {
                validate_next(&scan.records, &record)?;
                scan.records.push(record);
            }
            Ok(())
        })();
        if let Err(error) = result {
            scan.records.truncate(start);
            scan.diagnostic = Some(format!("line {}: {}", index + 1, error.message));
            break;
        }
    }
    scan
}

/// Serialize one all-or-nothing batch. Callers validate it against prior history.
pub fn encode_transaction(records: &[Record]) -> Result<Vec<u8>, Fault> {
    if records.is_empty() || records.iter().any(|record| record.schema_version != 2) {
        return Err(invalid("transactions require nonempty v2 records"));
    }
    let mut bytes = serde_json::to_vec(&Transaction {
        schema_version: 2,
        transaction: records.to_vec(),
    })
    .map_err(|error| invalid(error.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Return the durable active tree head and branch, excluding navigation records.
pub fn branch_state(records: &[Record]) -> Result<(Option<u64>, String), Fault> {
    validate_records(records)?;
    Ok(match records.last() {
        Some(record) if record.kind == "branch_selected" => {
            (record.parent_id, record.branch.clone())
        }
        Some(record) => (Some(record.sequence), record.branch.clone()),
        None => (None, "main".into()),
    })
}

/// Project the selected ancestor path. Navigation never becomes model context.
pub fn active_path(records: &[Record]) -> Result<Vec<Record>, Fault> {
    let (mut head, _) = branch_state(records)?;
    let mut path = Vec::new();
    while let Some(sequence) = head {
        let record = &records[sequence as usize - 1];
        path.push(record.clone());
        head = record.parent_id;
    }
    path.reverse();
    Ok(path)
}

/// Build a requested read view while retaining every tree identity. Full audit reads
/// remain byte-for-byte representable; these projections never enter persisted history.
pub fn view_records(records: &[Record], view: StoreView) -> Result<Vec<Record>, Fault> {
    if view == StoreView::Full {
        return Ok(records.to_vec());
    }
    if view == StoreView::Receipt {
        return Ok(Vec::new());
    }
    let (mut head, _) = branch_state(records)?;
    let mut last_input = None;
    if view == StoreView::Coding {
        while let Some(sequence) = head {
            let record = &records[sequence as usize - 1];
            if matches!(
                record.kind.as_str(),
                "model_request" | "model_request_revision"
            ) && record.payload.get("input").is_some()
            {
                last_input = Some(sequence);
                break;
            }
            head = record.parent_id;
        }
    }
    Ok(records
        .iter()
        .map(|record| {
            let payload = if matches!(
                record.kind.as_str(),
                "model_request" | "model_request_revision"
            ) && Some(record.sequence) != last_input
            {
                record
                    .payload
                    .as_object()
                    .map(|payload| {
                        serde_json::Value::Object(
                            payload
                                .iter()
                                .filter(|(key, _)| {
                                    !matches!(key.as_str(), "input" | "prompt_cache")
                                })
                                .map(|(key, value)| (key.clone(), value.clone()))
                                .collect(),
                        )
                    })
                    .unwrap_or_else(|| record.payload.clone())
            } else if matches!(record.kind.as_str(), "message" | "user_message")
                && record.payload["type"] == "message"
            {
                serde_json::Value::Object(
                    record
                        .payload
                        .as_object()
                        .into_iter()
                        .flatten()
                        .filter(|(key, _)| {
                            matches!(key.as_str(), "type" | "role" | "content" | "references")
                        })
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                )
            } else {
                record.payload.clone()
            };
            Record {
                schema_version: record.schema_version,
                session_id: record.session_id,
                sequence: record.sequence,
                parent_id: record.parent_id,
                branch: record.branch.clone(),
                run_id: record.run_id,
                kind: record.kind.clone(),
                payload,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn record(sequence: u64, parent_id: Option<u64>, kind: &str) -> Record {
        Record {
            schema_version: 2,
            session_id: 7,
            sequence,
            run_id: 1,
            parent_id,
            branch: "main".into(),
            kind: kind.into(),
            payload: json!({}),
        }
    }
    #[test]
    fn consumer_views_preserve_audit_and_select_the_current_ancestor_request() {
        let mut first = record(2, Some(1), "model_request");
        first.payload = json!({
            "input": { "items": ["OLD_AUDIT_INPUT"] },
            "context_edits": [1],
            "prompt_cache": { "version": 2 },
        });
        let mut departed = record(3, Some(2), "model_request");
        departed.payload =
            json!({ "input": { "items": ["DEPARTED_INPUT"] }, "context_edits": [2] });
        let mut selection = record(4, Some(2), "branch_selected");
        selection.branch = "alternate".into();
        selection.payload = json!({ "target": 2, "branch": "alternate" });
        let mut message = record(5, Some(2), "message");
        message.branch = "alternate".into();
        message.payload = json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "text", "text": "KEEP_ALL_TEXT" }],
            "references": ["KEEP_REFERENCE"],
            "audit_extra": "x".repeat(1024 * 1024),
        });
        let records = vec![
            record(1, None, "session"),
            first,
            departed,
            selection,
            message,
        ];
        let original = encode_transaction(&records).unwrap();
        let coding = view_records(&records, StoreView::Coding).unwrap();
        validate_records(&coding).unwrap();
        assert_eq!(coding.len(), records.len());
        assert_eq!(coding[1].payload["input"], records[1].payload["input"]);
        assert_eq!(
            coding[1].payload["prompt_cache"],
            records[1].payload["prompt_cache"]
        );
        assert!(coding[2].payload.get("input").is_none());
        assert_eq!(coding[2].payload["context_edits"], json!([2]));
        assert_eq!(coding[4].payload["references"], json!(["KEEP_REFERENCE"]));
        assert_eq!(coding[4].payload["content"], records[4].payload["content"]);
        assert!(serde_json::to_vec(&coding).unwrap().len() < 4096);
        let presentation = view_records(&records, StoreView::Presentation).unwrap();
        assert!(presentation[1].payload.get("input").is_none());
        assert_eq!(
            encode_transaction(&view_records(&records, StoreView::Full).unwrap()).unwrap(),
            original
        );
        assert_eq!(encode_transaction(&records).unwrap(), original);
    }

    #[test]
    fn optional_store_reply_view_retains_the_legacy_operation_wire_contract() {
        let request = crate::coding::StoreRequest::AppendChecked {
            run_id: 7,
            session_id: 9,
            sequence: 12,
            head: Some(10),
            branch: "work".into(),
            new_branch: None,
            entries: vec![crate::coding::RecordDraft {
                kind: "message".into(),
                payload: json!({ "text": "intent" }),
            }],
        };
        let legacy = serde_json::to_value(&request).unwrap();
        let optional = serde_json::to_value(request.with_view(StoreView::Receipt)).unwrap();
        assert_eq!(optional["_eden_history_view"], "receipt");
        let decoded: crate::coding::StoreRequest = serde_json::from_value(optional).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), legacy);
    }
    #[test]
    fn incomplete_transaction_exposes_only_previously_committed_prefix() {
        let mut bytes = encode_transaction(&[record(1, None, "user")]).unwrap();
        let tail = encode_transaction(&[
            record(2, Some(1), "assistant"),
            record(3, Some(2), "tool_intent"),
        ])
        .unwrap();
        bytes.extend_from_slice(&tail[..tail.len() - 2]);
        let scan = scan_records(&bytes);
        assert_eq!(scan.records.len(), 1);
        assert!(scan.diagnostic.unwrap().contains("line 2"));
    }
    #[test]
    fn invalid_transaction_never_partially_commits_its_records() {
        let mut bytes = encode_transaction(&[record(1, None, "user")]).unwrap();
        bytes.extend(
            serde_json::to_vec(&json!({
                "schema_version": 2,
                "transaction": [record(2, Some(1), "assistant"), record(4, Some(2), "tool_intent")],
            }))
            .unwrap(),
        );
        bytes.push(b'\n');
        let scan = scan_records(&bytes);
        assert_eq!(scan.records.len(), 1);
        assert!(scan.diagnostic.is_some());
    }
    #[test]
    fn navigation_changes_active_path_without_deleting_old_nodes() {
        let mut selected = record(3, Some(1), "branch_selected");
        selected.branch = "alternate".into();
        selected.payload = json!({ "target": 1, "branch": "alternate" });
        let mut next = record(4, Some(1), "assistant");
        next.branch = "alternate".into();
        let records = vec![
            record(1, None, "user"),
            record(2, Some(1), "assistant"),
            selected,
            next,
        ];
        let scan = scan_records(&encode_transaction(&records).unwrap());
        assert!(scan.diagnostic.is_none());
        assert_eq!(scan.records.len(), 4);
        assert_eq!(
            branch_state(&scan.records).unwrap(),
            (Some(4), "alternate".into())
        );
        assert_eq!(
            active_path(&scan.records)
                .unwrap()
                .iter()
                .map(|r| r.sequence)
                .collect::<Vec<_>>(),
            vec![1, 4]
        );
    }
    #[test]
    fn v1_is_readable_with_linear_parent_projection() {
        let text = concat!(
            "{\"schema_version\":1,\"session_id\":7,\"sequence\":1,\"run_id\":1,\"kind\":\"user\",\
             \"payload\":{}}\n",
            "{\"schema_version\":1,\"session_id\":7,\"sequence\":2,\"run_id\":1,\"kind\":\"\
             assistant\",\"payload\":{}}\n",
        );
        let bytes = text.as_bytes();
        let scan = scan_records(bytes);
        assert!(scan.diagnostic.is_none());
        assert_eq!(scan.records[1].parent_id, Some(1));
        assert_eq!(active_path(&scan.records).unwrap().len(), 2);
    }
    #[test]
    fn middle_damage_never_skips_to_later_records() {
        let mut bytes = encode_transaction(&[record(1, None, "user")]).unwrap();
        bytes.extend_from_slice(b"broken\n");
        bytes.extend(encode_transaction(&[record(2, Some(1), "assistant")]).unwrap());
        assert_eq!(scan_records(&bytes).records.len(), 1);
    }
}
