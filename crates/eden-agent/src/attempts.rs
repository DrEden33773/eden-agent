//! Preserve unsuccessful provider text after the cancelled invocation has drained.
use crate::{Event, Terminal};
use serde_json::Value;

pub(crate) fn records(
    events: &[Event],
    run_id: u64,
    terminal: &Terminal,
    history: &[eden_protocol::coding::Record],
) -> Vec<Value> {
    let mut attempts = std::collections::BTreeMap::<String, Value>::new();
    let mut request_id = Value::Null;
    for event in events.iter().filter(|event| event.run_id == run_id) {
        if event.kind == "model_request" {
            request_id = event.payload["request_id"].clone();
        }
        let Some(id) = event.payload["attempt_id"].as_str() else {
            continue;
        };
        match event.kind.as_str() {
            "model_attempt_started" => {
                attempts.insert(
                    id.into(),
                    serde_json::json!({
                        "attempt_id": id,
                        "request_id": request_id,
                        "request_sequence": event.sequence,
                        "text": "",
                        "status": "interrupted",
                    }),
                );
            }
            "model_text_delta" => {
                if let Some(attempt) = attempts.get_mut(id)
                    && let Some(delta) = event.payload["delta"].as_str()
                {
                    let mut text = attempt["text"].as_str().unwrap_or_default().to_owned();
                    text.push_str(delta);
                    attempt["text"] = Value::String(text);
                }
            }
            "model_attempt_finished" => {
                if let Some(attempt) = attempts.get_mut(id) {
                    attempt["status"] = event.payload["status"].clone();
                    attempt["error"] = event.payload["error"].clone();
                    if event.payload["text"]
                        .as_str()
                        .is_some_and(|text| !text.is_empty())
                    {
                        attempt["text"] = event.payload["text"].clone();
                    }
                }
            }
            _ => {}
        }
    }
    attempts
        .into_values()
        .filter_map(|mut attempt| {
            if attempt["status"] == "completed" {
                let committed = history.iter().any(|record| {
                    matches!(
                        record.kind.as_str(),
                        "model_response" | "compaction" | "branch_summary"
                    ) && record.payload["request_id"].is_string()
                        && record.payload["request_id"] == attempt["request_id"]
                });
                if committed {
                    return None;
                }
                attempt["status"] = "interrupted".into();
            }
            if attempt["status"] == "interrupted" {
                match &terminal.outcome {
                    crate::Outcome::Cancelled => attempt["status"] = "cancelled".into(),
                    crate::Outcome::Failed(error) => {
                        attempt["status"] = "failed".into();
                        attempt["error"] = serde_json::json!(error);
                    }
                    crate::Outcome::Completed(_) => {}
                }
            }
            Some(attempt)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Outcome;
    use serde_json::json;

    fn event(sequence: u64, kind: &str, payload: Value) -> Event {
        Event {
            sequence,
            session_id: 1,
            run_id: 7,
            kind: kind.into(),
            payload,
        }
    }

    #[test]
    fn cancelled_partial_text_survives_without_executable_tool_arguments() {
        let events = vec![
            event(0, "model_request", json!({ "request_id": "request-1" })),
            event(1, "model_attempt_started", json!({ "attempt_id": "7:1" })),
            event(
                2,
                "model_text_delta",
                json!({ "attempt_id": "7:1", "delta": "partial" }),
            ),
            event(
                3,
                "model_tool_delta",
                json!({ "attempt_id": "7:1", "delta": "unsafe partial arguments" }),
            ),
        ];
        let saved = records(
            &events,
            7,
            &Terminal {
                outcome: Outcome::Cancelled,
                cleanup_errors: vec![],
            },
            &[],
        );
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0]["text"], "partial");
        assert_eq!(saved[0]["status"], "cancelled");
        assert!(!saved[0].to_string().contains("unsafe"));
        assert!(
            records(
                &events,
                8,
                &Terminal {
                    outcome: Outcome::Cancelled,
                    cleanup_errors: vec![]
                },
                &[]
            )
            .is_empty()
        );
    }

    #[test]
    fn retries_keep_failed_attempt_but_exclude_completed_attempt() {
        let events = vec![
            event(0, "model_request", json!({ "request_id": "request-1" })),
            event(1, "model_attempt_started", json!({ "attempt_id": "7:1" })),
            event(
                2,
                "model_text_delta",
                json!({ "attempt_id": "7:1", "delta": "first" }),
            ),
            event(
                3,
                "model_attempt_finished",
                json!({
                    "attempt_id": "7:1",
                    "status": "failed",
                    "error": { "code": "Interrupted" },
                }),
            ),
            event(4, "model_attempt_started", json!({ "attempt_id": "7:2" })),
            event(
                5,
                "model_text_delta",
                json!({ "attempt_id": "7:2", "delta": "done" }),
            ),
            event(
                6,
                "model_attempt_finished",
                json!({ "attempt_id": "7:2", "status": "completed" }),
            ),
        ];
        let saved = records(
            &events,
            7,
            &Terminal {
                outcome: Outcome::Completed(Value::Null),
                cleanup_errors: vec![],
            },
            &[],
        );
        assert_eq!(saved.len(), 2, "completed but uncommitted text is retained");
        assert_eq!(saved[1]["text"], "done");
        assert_eq!(saved[0]["attempt_id"], "7:1");
        assert_eq!(saved[0]["error"]["code"], "Interrupted");
        let committed: eden_protocol::coding::Record = serde_json::from_value(json!({
            "schema_version": 1,
            "session_id": 1,
            "sequence": 1,
            "run_id": 7,
            "kind": "model_response",
            "payload": { "request_id": "request-1" },
        }))
        .unwrap();
        let saved = records(
            &events,
            7,
            &Terminal {
                outcome: Outcome::Completed(Value::Null),
                cleanup_errors: vec![],
            },
            &[committed],
        );
        assert_eq!(
            saved.len(),
            1,
            "a committed successful retry is not a failed attempt"
        );
        assert_eq!(saved[0]["attempt_id"], "7:1");
    }
}
