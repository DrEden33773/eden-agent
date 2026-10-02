//! Consumer reads share strict committed-prefix validation while leaving audit bytes on disk.
use crate::{
    Fault,
    coding::{Record, StoreView},
    history::HistoryScan,
};
use serde::{
    Deserialize,
    de::{DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value};
use std::io::{BufRead, BufReader, Read};

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

struct PayloadSeed<const MODE: u8> {
    audit: bool,
}
impl<'de, const MODE: u8> DeserializeSeed<'de> for PayloadSeed<MODE> {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        struct Fields<const MODE: u8> {
            audit: bool,
        }
        impl<'de, const MODE: u8> Visitor<'de> for Fields<MODE> {
            type Value = Value;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a public history payload")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
                let mut payload = Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if (MODE != 0
                        && !(self.audit && matches!(key.as_str(), "input" | "prompt_cache")))
                        || (MODE == 0
                            && matches!(
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
                            ))
                    {
                        payload.insert(key, map.next_value()?);
                    } else {
                        map.next_value::<Discard>()?;
                    }
                }
                Ok(Value::Object(payload))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut values: A) -> Result<Value, A::Error> {
                if MODE == 0 {
                    while values.next_element::<Discard>()?.is_some() {}
                    Ok(Value::Null)
                } else {
                    let mut result = Vec::new();
                    while let Some(value) = values.next_element::<Value>()? {
                        result.push(value);
                    }
                    Ok(Value::Array(result))
                }
            }
            fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
                Ok(if MODE == 0 {
                    Value::Null
                } else {
                    Value::from(value)
                })
            }
            fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
                Ok(if MODE == 0 {
                    Value::Null
                } else {
                    Value::from(value)
                })
            }
            fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
                Ok(if MODE == 0 {
                    Value::Null
                } else {
                    Value::from(value)
                })
            }
            fn visit_f64<E>(self, value: f64) -> Result<Value, E> {
                Ok(if MODE == 0 {
                    Value::Null
                } else {
                    Value::from(value)
                })
            }
            fn visit_str<E>(self, value: &str) -> Result<Value, E> {
                Ok(if MODE == 0 {
                    Value::Null
                } else {
                    Value::from(value)
                })
            }
            fn visit_unit<E>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }
        }
        deserializer.deserialize_any(Fields::<MODE> { audit: self.audit })
    }
}

// Legacy lines and transaction nodes use the same validating skip for extension
// fields; derived unknown-field handling would accept malformed string escapes.
// RawValue's iterative skip does not enforce the complete reader's recursion budget.
// Payload validation starts a fresh deserializer, so include its whole-line enclosing containers.
fn validate_payload_depth(
    raw: &serde_json::value::RawValue,
    enclosing: usize,
) -> Result<(), serde_json::Error> {
    let mut depth = enclosing;
    let mut quoted = false;
    let mut escaped = false;
    for &byte in raw.get().as_bytes() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'{' | b'[' => {
                    depth += 1;
                    if depth >= 128 {
                        return Err(serde::de::Error::custom("recursion limit exceeded"));
                    }
                }
                b'}' | b']' => depth -= 1,
                _ => {}
            }
        }
    }
    Ok(())
}
fn node_value<'de, const MODE: u8, A: MapAccess<'de>>(
    key: &str,
    map: &mut A,
    payload: &mut Option<&'de serde_json::value::RawValue>,
    enclosing: usize,
) -> Result<Option<Value>, A::Error> {
    match key {
        "payload" => {
            if MODE == 0 {
                // Summary fields do not depend on kind or map ordering; validate once.
                return map
                    .next_value_seed(PayloadSeed::<0> { audit: false })
                    .map(Some);
            }
            // Validate duplicate values too: the complete parser does not ignore their Unicode.
            if let Some(previous) = payload.take() {
                serde_json::from_str::<Discard>(previous.get())
                    .map_err(serde::de::Error::custom)?;
            }
            let raw = map.next_value::<&serde_json::value::RawValue>()?;
            validate_payload_depth(raw, enclosing).map_err(serde::de::Error::custom)?;
            *payload = Some(raw);
            Ok(None)
        }
        "schema_version" | "session_id" | "sequence" | "run_id" | "parent_id" | "branch"
        | "kind" => Ok(Some(map.next_value()?)),
        _ => {
            map.next_value::<Discard>()?;
            Ok(None)
        }
    }
}
fn finish_payload<const MODE: u8>(
    node: &mut Map<String, Value>,
    payload: Option<&serde_json::value::RawValue>,
) -> Result<(), serde_json::Error> {
    if let Some(payload) = payload {
        let audit = matches!(
            node.get("kind").and_then(Value::as_str),
            Some("model_request" | "model_request_revision")
        );
        let mut deserializer = serde_json::Deserializer::from_str(payload.get());
        let value = PayloadSeed::<MODE> { audit }.deserialize(&mut deserializer)?;
        deserializer.end()?;
        node.insert("payload".into(), value);
    }
    Ok(())
}
struct ProjectedRecord<const MODE: u8>(Record);
impl<'de, const MODE: u8> Deserialize<'de> for ProjectedRecord<MODE> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Fields<const MODE: u8>;
        impl<'de, const MODE: u8> Visitor<'de> for Fields<MODE> {
            type Value = ProjectedRecord<MODE>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a public history node")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut node = Map::new();
                let mut payload = None;
                while let Some(key) = map.next_key::<String>()? {
                    if let Some(value) = node_value::<MODE, _>(&key, &mut map, &mut payload, 3)? {
                        node.insert(key, value);
                    }
                }
                finish_payload::<MODE>(&mut node, payload).map_err(serde::de::Error::custom)?;
                serde_json::from_value(Value::Object(node))
                    .map(ProjectedRecord)
                    .map_err(serde::de::Error::custom)
            }
        }
        deserializer.deserialize_map(Fields::<MODE>)
    }
}
struct Line<'a, const MODE: u8> {
    header: Map<String, Value>,
    transaction: Option<&'a serde_json::value::RawValue>,
    extra: bool,
}
impl<'de, const MODE: u8> Deserialize<'de> for Line<'de, MODE> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Fields<const MODE: u8>;
        impl<'de, const MODE: u8> Visitor<'de> for Fields<MODE> {
            type Value = Line<'de, MODE>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a public history line")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut line = Line {
                    header: Map::new(),
                    transaction: None,
                    extra: false,
                };
                let mut payload = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "transaction" => {
                            // JSON parsing validates every value; record semantics belong only
                            // to the last duplicate transaction, exactly like the full reader.
                            if let Some(previous) = line.transaction.take() {
                                serde_json::from_str::<Discard>(previous.get())
                                    .map_err(serde::de::Error::custom)?;
                            }
                            let raw = map.next_value::<&serde_json::value::RawValue>()?;
                            validate_payload_depth(raw, 1).map_err(serde::de::Error::custom)?;
                            line.transaction = Some(raw);
                        }
                        "payload" => {
                            if let Some(value) =
                                node_value::<MODE, _>(&key, &mut map, &mut payload, 1)?
                            {
                                line.header.insert(key, value);
                            }
                        }
                        _ => {
                            if let Some(value) =
                                node_value::<MODE, _>(&key, &mut map, &mut payload, 1)?
                            {
                                line.header.insert(key, value);
                            } else {
                                line.extra = true;
                            }
                        }
                    }
                }
                finish_payload::<MODE>(&mut line.header, payload)
                    .map_err(serde::de::Error::custom)?;
                Ok(line)
            }
        }
        deserializer.deserialize_map(Fields::<MODE>)
    }
}

fn transaction<const MODE: u8>(line: &[u8], prefix: &[Record]) -> Result<Vec<Record>, Fault> {
    if !line.ends_with(b"\n") {
        return Err(invalid("incomplete history tail; source preserved"));
    }
    let line: Line<'_, MODE> = serde_json::from_slice(line)
        .map_err(|error| invalid(format!("invalid public history JSON: {error}")))?;
    if let Some(raw) = line.transaction {
        let records: Vec<ProjectedRecord<MODE>> =
            serde_json::from_str(raw.get()).map_err(|error| invalid(error.to_string()))?;
        if line.header.get("schema_version") != Some(&Value::from(2))
            || line.extra
            || line.header.len() != 1
            || records.is_empty()
            || records.iter().any(|record| record.0.schema_version != 2)
        {
            return Err(invalid("invalid transaction schema or empty transaction"));
        }
        Ok(records.into_iter().map(|record| record.0).collect())
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

fn inspect<const MODE: u8>(
    file: impl Read,
    cancelled: &impl Fn() -> bool,
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
            if cancelled() {
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
        let result = transaction::<MODE>(&line, &scan.records).and_then(|records| {
            for record in records {
                crate::history::validate_next(&scan.records, &record)?;
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
    if cancelled() {
        return Ok(None);
    }
    Ok(Some(scan))
}

/// The preparation view retains binding and configuration records; execution stores still open full audit input.
#[derive(Clone, Copy)]
pub enum ReadView {
    /// Catalog identity, tree and display metadata without audit allocation.
    Summary,
    /// Binding and configuration preflight before loading a saved composition.
    Preparation,
    /// The same consumer projection as `StoreView::Presentation`.
    Presentation,
}
/// Validate every JSON value and transaction before exposing a consumer view of its committed prefix.
/// Cancellation returns no result; damage returns the same prefix boundary as the complete reader.
pub fn read_view(
    file: impl Read,
    view: ReadView,
    cancelled: &impl Fn() -> bool,
) -> Result<Option<HistoryScan>, Fault> {
    match view {
        ReadView::Summary => inspect::<0>(file, cancelled),
        ReadView::Preparation => inspect::<1>(file, cancelled),
        ReadView::Presentation => {
            let mut scan = inspect::<1>(file, cancelled)?;
            if let Some(scan) = &mut scan {
                scan.records =
                    crate::history::view_records(&scan.records, StoreView::Presentation)?;
            }
            Ok(scan)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{encode_transaction, scan_records, view_records};
    use serde_json::json;
    fn record(sequence: u64, kind: &str, payload: Value) -> Record {
        Record {
            schema_version: 2,
            session_id: 17,
            sequence,
            run_id: 0,
            parent_id: (sequence > 1).then_some(sequence - 1),
            branch: "main".into(),
            kind: kind.into(),
            payload,
        }
    }
    fn scan(bytes: &[u8], view: ReadView) -> HistoryScan {
        read_view(bytes, view, &|| false).unwrap().unwrap()
    }
    #[test]
    fn presentation_matches_full_projection_and_preparation_keeps_binding() {
        let records = vec![
            record(1, "session", json!({ "cwd": "/isolated" })),
            record(
                2,
                "composition_lock",
                json!({ "packages": [{ "digest": "identity" }], "roles": {} }),
            ),
            record(
                3,
                "model_request",
                json!({
                    "input": { "messages": [{ "content": [{ "text": "previous message" }] }] },
                    "prompt_cache": { "secret": "fixture" },
                    "attempt_id": "a",
                }),
            ),
            record(
                4,
                "message",
                json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "回答" }],
                    "references": [],
                    "extra": "audit",
                }),
            ),
            record(
                5,
                "branch_selected",
                json!({ "target": 4, "branch": "main" }),
            ),
        ];
        let bytes = encode_transaction(&records).unwrap();
        let projected = scan(&bytes, ReadView::Presentation);
        assert_eq!(
            json!(projected.records),
            json!(view_records(&records, StoreView::Presentation).unwrap())
        );
        let prepared = scan(&bytes, ReadView::Preparation);
        assert_eq!(json!(prepared.records[1]), json!(records[1]));
        assert!(prepared.records[2].payload.get("input").is_none());
        assert_eq!(json!(scan_records(&bytes).records), json!(records));
    }
    #[test]
    fn damaged_unicode_and_transactions_have_the_same_committed_prefix() {
        let prefix =
            encode_transaction(&[record(1, "session", json!({ "cwd": "/fixture" }))]).unwrap();
        let tail = encode_transaction(&[
            record(
                2,
                "model_request",
                json!({ "input": { "data": "sentinel" } }),
            ),
            record(3, "message", json!({ "type": "message", "content": [] })),
        ])
        .unwrap();
        for replacement in [r"\uD800", r"\uDC00", r"\x00"] {
            for field in ["input", "extension", "payload"] {
                let mut bytes = prefix.clone();
                let tail = String::from_utf8(tail.clone())
                    .unwrap()
                    .replace("sentinel", replacement)
                    .replace("input", field);
                bytes.extend_from_slice(tail.as_bytes());
                let full = scan_records(&bytes);
                for view in [
                    ReadView::Summary,
                    ReadView::Preparation,
                    ReadView::Presentation,
                ] {
                    let projected = scan(&bytes, view);
                    assert_eq!(projected.records.len(), full.records.len());
                    assert!(projected.diagnostic.is_some());
                }
            }
        }
        let mut incomplete = prefix.clone();
        incomplete.extend_from_slice(&tail[..tail.len() - 1]);
        assert_eq!(
            json!(scan(&incomplete, ReadView::Presentation).records),
            json!(scan_records(&incomplete).records)
        );
        assert!(
            read_view(prefix.as_slice(), ReadView::Presentation, &|| true)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn payload_before_kind_and_duplicate_invalid_payload_are_validated() {
        let bytes = b"{\"schema_version\":2,\"transaction\":[{\"payload\":{\"cwd\":\"/fixture\"},\"schema_version\":2,\"session_id\":7,\"sequence\":1,\"run_id\":0,\"branch\":\"main\",\"kind\":\"session\"}]}\n";
        assert_eq!(
            json!(scan(bytes, ReadView::Presentation).records),
            json!(scan_records(bytes).records)
        );
        let bytes = String::from_utf8(bytes.to_vec()).unwrap().replace(
            "\"payload\":",
            r#""payload":{"ignored":"\uD800"},"payload":"#,
        );
        assert!(
            scan(bytes.as_bytes(), ReadView::Presentation)
                .diagnostic
                .is_some()
        );
        assert!(scan_records(bytes.as_bytes()).diagnostic.is_some());
    }
    #[test]
    fn projection_preserves_the_complete_readers_nesting_boundary() {
        for schema in [1, 2] {
            for depth in 120..132 {
                let mut audit = Value::Null;
                for _ in 0..depth {
                    audit = json!([audit]);
                }
                let mut header = record(1, "session", json!({ "cwd": "/fixture" }));
                let mut request = record(2, "model_request", json!({ "input": audit }));
                header.schema_version = schema;
                request.schema_version = schema;
                let mut bytes = if schema == 2 {
                    encode_transaction(&[header]).unwrap()
                } else {
                    let mut bytes = serde_json::to_vec(&header).unwrap();
                    bytes.push(b'\n');
                    bytes
                };
                let tail = if schema == 2 {
                    encode_transaction(&[request]).unwrap()
                } else {
                    let mut bytes = serde_json::to_vec(&request).unwrap();
                    bytes.push(b'\n');
                    bytes
                };
                bytes.extend_from_slice(&tail);
                let full = scan_records(&bytes);
                for view in [
                    ReadView::Summary,
                    ReadView::Preparation,
                    ReadView::Presentation,
                ] {
                    let projected = scan(&bytes, view);
                    assert_eq!(
                        projected.records.len(),
                        full.records.len(),
                        "schema={schema} depth={depth}"
                    );
                    assert_eq!(projected.diagnostic.is_some(), full.diagnostic.is_some());
                }
            }
        }
    }
    #[test]
    fn overwritten_transaction_values_keep_the_full_readers_semantics() {
        let record = record(1, "session", json!({ "cwd": "/fixture" }));
        let final_value = serde_json::to_string(&vec![record]).unwrap();
        for overwritten in ["null", "17", "true", r#""text""#, r#"[{"sequence":"bad"}]"#] {
            let bytes = format!(
                "{{\"schema_version\":2,\"transaction\":{overwritten},\"transaction\":\
                 {final_value}}}\n"
            )
            .into_bytes();
            let full = scan_records(&bytes);
            assert!(full.diagnostic.is_none());
            for view in [
                ReadView::Summary,
                ReadView::Preparation,
                ReadView::Presentation,
            ] {
                let projected = scan(&bytes, view);
                assert_eq!(json!(projected.records), json!(full.records));
                assert_eq!(projected.diagnostic.is_some(), full.diagnostic.is_some());
            }
        }
    }
}
