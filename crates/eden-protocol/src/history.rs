//! Public history validation and active-branch projection, independent of native plugins.

use crate::{Fault, coding::Record};
use serde::{Deserialize, Serialize};

/// A validated committed prefix and, if present, the first damage diagnostic.
/// The original bytes are never changed, and records after damage are never used.
#[derive(Clone, Debug)]
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

fn validate_next(records: &[Record], record: &Record) -> Result<(), Fault> {
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
        let bytes = b"{\"schema_version\":1,\"session_id\":7,\"sequence\":1,\
                     \"run_id\":1,\"kind\":\"user\",\
         \"payload\":{}}\n{\"schema_version\":1,\"session_id\":7,\"sequence\":2,\"run_id\":1,\
         \"kind\":\"assistant\",\"payload\":{}}\n";
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
