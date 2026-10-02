//! Directory summaries validate committed framing and tree headers without allocating audit inputs.
use eden_protocol::{Fault, coding::Record, history::HistoryScan};
use serde::{
    Deserialize,
    de::{MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value};
use std::{
    fs::File,
    io::{BufRead, BufReader},
};

fn invalid(message: impl Into<String>) -> Fault {
    Fault::new("PersistenceFailure", "public-history", message)
}

// deserialize_any validates UTF-8, surrogate escapes, numbers and nesting just
// like Value. IgnoredAny's fast skip would accept damaged audit strings.
struct Discard;
impl<'de> Deserialize<'de> for Discard {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Values;
        impl<'de> Visitor<'de> for Values {
            type Value = Discard;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a valid JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Discard, A::Error> {
                while map.next_key::<Discard>()?.is_some() {
                    map.next_value::<Discard>()?;
                }
                Ok(Discard)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut values: A) -> Result<Discard, A::Error> {
                while values.next_element::<Discard>()?.is_some() {}
                Ok(Discard)
            }
            fn visit_bool<E>(self, _: bool) -> Result<Discard, E> {
                Ok(Discard)
            }
            fn visit_i64<E>(self, _: i64) -> Result<Discard, E> {
                Ok(Discard)
            }
            fn visit_u64<E>(self, _: u64) -> Result<Discard, E> {
                Ok(Discard)
            }
            fn visit_f64<E>(self, _: f64) -> Result<Discard, E> {
                Ok(Discard)
            }
            fn visit_str<E>(self, _: &str) -> Result<Discard, E> {
                Ok(Discard)
            }
            fn visit_unit<E>(self) -> Result<Discard, E> {
                Ok(Discard)
            }
        }
        deserializer.deserialize_any(Values)
    }
}

struct Payload(Value);
impl<'de> Deserialize<'de> for Payload {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Fields;
        impl<'de> Visitor<'de> for Fields {
            type Value = Payload;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a public history payload")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Payload, A::Error> {
                let mut payload = Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if matches!(
                        key.as_str(),
                        "cwd"
                            | "name"
                            | "tags"
                            | "type"
                            | "role"
                            | "content"
                            | "command"
                            | "intent"
                            | "timestamp_ms"
                            | "activity_unix"
                            | "selection"
                            | "origin"
                            | "target"
                            | "branch"
                    ) {
                        payload.insert(key, map.next_value()?);
                    } else {
                        map.next_value::<Discard>()?;
                    }
                }
                Ok(Payload(Value::Object(payload)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut values: A) -> Result<Payload, A::Error> {
                while values.next_element::<Discard>()?.is_some() {}
                Ok(Payload(Value::Null))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Payload, E> {
                Ok(Payload(Value::Null))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Payload, E> {
                Ok(Payload(Value::Null))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Payload, E> {
                Ok(Payload(Value::Null))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Payload, E> {
                Ok(Payload(Value::Null))
            }
            fn visit_str<E>(self, _: &str) -> Result<Payload, E> {
                Ok(Payload(Value::Null))
            }
            fn visit_unit<E>(self) -> Result<Payload, E> {
                Ok(Payload(Value::Null))
            }
        }
        deserializer.deserialize_any(Fields)
    }
}

#[derive(Deserialize)]
struct SummaryRecord {
    schema_version: u32,
    session_id: u64,
    sequence: u64,
    run_id: u64,
    #[serde(default)]
    parent_id: Option<u64>,
    #[serde(default = "main_branch")]
    branch: String,
    kind: String,
    payload: Payload,
}
fn main_branch() -> String {
    "main".into()
}
impl From<SummaryRecord> for Record {
    fn from(record: SummaryRecord) -> Self {
        Self {
            schema_version: record.schema_version,
            session_id: record.session_id,
            sequence: record.sequence,
            run_id: record.run_id,
            parent_id: record.parent_id,
            branch: record.branch,
            kind: record.kind,
            payload: record.payload.0,
        }
    }
}

struct Line {
    header: Map<String, Value>,
    transaction: Option<Vec<SummaryRecord>>,
    extra: bool,
}
impl<'de> Deserialize<'de> for Line {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Fields;
        impl<'de> Visitor<'de> for Fields {
            type Value = Line;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a public history line")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Line, A::Error> {
                let mut line = Line {
                    header: Map::new(),
                    transaction: None,
                    extra: false,
                };
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "transaction" => line.transaction = Some(map.next_value()?),
                        "payload" => {
                            line.header.insert(key, map.next_value::<Payload>()?.0);
                        }
                        "schema_version" | "session_id" | "sequence" | "run_id" | "parent_id"
                        | "branch" | "kind" => {
                            line.header.insert(key, map.next_value()?);
                        }
                        _ => {
                            map.next_value::<Discard>()?;
                            line.extra = true;
                        }
                    }
                }
                Ok(line)
            }
        }
        deserializer.deserialize_map(Fields)
    }
}

fn transaction(line: &[u8], prefix: &[Record]) -> Result<Vec<Record>, Fault> {
    if !line.ends_with(b"\n") {
        return Err(invalid("incomplete history tail; source preserved"));
    }
    let line: Line = serde_json::from_slice(line)
        .map_err(|error| invalid(format!("invalid public history JSON: {error}")))?;
    if let Some(records) = line.transaction {
        if line.header.get("schema_version") != Some(&Value::from(2))
            || line.extra
            || line.header.len() != 1
            || records.is_empty()
            || records.iter().any(|record| record.schema_version != 2)
        {
            return Err(invalid("invalid transaction schema or empty transaction"));
        }
        Ok(records.into_iter().map(Record::from).collect())
    } else {
        let mut record: Record = serde_json::from_value(Value::Object(line.header))
            .map_err(|error| invalid(error.to_string()))?;
        if record.schema_version != 1 {
            return Err(invalid(
                "v2 records require a committed transaction envelope",
            ));
        }
        record.parent_id = prefix.last().map(|record| record.sequence);
        record.branch = "main".into();
        Ok(vec![record])
    }
}

pub(crate) fn inspect(
    file: File,
    cancel: &eden_plugin_sdk::Cancellation,
) -> Result<Option<HistoryScan>, Fault> {
    let mut reader = BufReader::with_capacity(65536, file);
    let mut scan = HistoryScan {
        records: vec![],
        diagnostic: None,
    };
    let mut index = 0;
    loop {
        let mut line = Vec::new();
        loop {
            if cancel.is_cancelled() {
                return Ok(None);
            }
            let available = reader
                .fill_buf()
                .map_err(|error| invalid(error.to_string()))?;
            if available.is_empty() {
                break;
            }
            let take = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |position| position + 1);
            let complete = available[take - 1] == b'\n';
            line.extend_from_slice(&available[..take]);
            reader.consume(take);
            if complete {
                break;
            }
        }
        if line.is_empty() {
            break;
        }
        index += 1;
        let start = scan.records.len();
        let result = transaction(&line, &scan.records).and_then(|records| {
            for record in records {
                eden_protocol::history::validate_next(&scan.records, &record)?;
                scan.records.push(record);
            }
            Ok(())
        });
        if let Err(error) = result {
            scan.records.truncate(start);
            scan.diagnostic = Some(format!("line {index}: {}", error.message));
            break;
        }
    }
    if cancel.is_cancelled() {
        return Ok(None);
    }
    Ok(Some(scan))
}

#[cfg(test)]
mod tests {
    use super::*;
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
