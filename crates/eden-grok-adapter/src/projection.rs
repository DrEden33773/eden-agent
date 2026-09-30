//! Ordered Eden commits become ACP presentation updates; execution remains in the host.
use eden_tui_client::Snapshot;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(crate) struct Projection {
    head: u64,
    closed_attempts: BTreeSet<(u64, String)>,
    closed_runs: BTreeSet<u64>,
    event: u64,
    tools: BTreeMap<String, (String, Value)>,
    streamed: BTreeMap<(u64, String), String>,
}

pub(crate) fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

fn chunk(kind: &str, value: &str) -> Value {
    json!({ "sessionUpdate": kind, "content": { "type": "text", "text": value } })
}

impl Projection {
    pub(crate) fn apply(&mut self, snapshot: &Snapshot, replay: bool) -> Vec<Value> {
        let mut updates = Vec::new();
        let start = snapshot
            .history
            .partition_point(|record| record.sequence <= self.head);
        let fresh = &snapshot.history[start..];
        let records: BTreeMap<_, _> = fresh
            .iter()
            .map(|record| (record.sequence, record))
            .collect();
        if !replay {
            for event in &snapshot.events {
                if event.sequence <= self.event {
                    continue;
                }
                self.event = event.sequence;
                match event.kind.as_str() {
                    "committed" => {
                        if let Some(record) = event.payload["sequence"]
                            .as_u64()
                            .and_then(|seq| records.get(&seq))
                        {
                            self.record(record, false, &mut updates);
                        }
                    }
                    _ => self.observe(event, &mut updates),
                }
            }
        } else {
            self.event = snapshot.events.last().map_or(0, |e| e.sequence);
        }
        for record in fresh {
            self.record(record, replay, &mut updates);
        }
        if replay && let Some(run) = snapshot.state.active_run {
            let durable_attempts: BTreeSet<_> = snapshot
                .history
                .iter()
                .filter(|r| r.run_id == run && r.kind == "model_attempt")
                .filter_map(|r| r.payload["attempt_id"].as_str())
                .collect();
            let committed = snapshot
                .events
                .iter()
                .filter(|event| {
                    event.run_id == run
                        && event.kind == "committed"
                        && event.payload["kind"] == "model_response"
                })
                .map(|event| event.sequence)
                .max()
                .unwrap_or(0);
            for event in snapshot.events.iter().filter(|event| event.run_id == run) {
                if event.sequence > committed
                    && !event.payload["attempt_id"]
                        .as_str()
                        .is_some_and(|id| durable_attempts.contains(id))
                {
                    self.observe(event, &mut updates);
                }
            }
        }
        updates
    }

    fn observe(&mut self, event: &eden_protocol::Event, updates: &mut Vec<Value>) {
        let attempt = text(&event.payload["attempt_id"]).to_owned();
        if event.kind == "model_attempt_finished" {
            self.closed_attempts.insert((event.run_id, attempt));
        } else if matches!(
            event.kind.as_str(),
            "model_text_delta" | "model_reasoning_delta"
        ) {
            if self.closed_runs.contains(&event.run_id)
                || self.closed_attempts.contains(&(event.run_id, attempt))
            {
                return;
            }
            let kind = if event.kind == "model_text_delta" {
                "agent_message_chunk"
            } else {
                "agent_thought_chunk"
            };
            let delta = text(&event.payload["delta"]);
            self.streamed
                .entry((event.run_id, kind.into()))
                .or_default()
                .push_str(delta);
            let mut update = chunk(kind, delta);
            update["_eden_run"] = json!(event.run_id);
            update["_eden_attempt"] = event.payload["attempt_id"].clone();
            updates.push(update);
        }
    }

    fn record(
        &mut self,
        record: &eden_protocol::coding::Record,
        replay: bool,
        updates: &mut Vec<Value>,
    ) {
        if record.sequence <= self.head {
            return;
        }
        self.head = record.sequence;
        let start = updates.len();
        let value = &record.payload;
        let run = record.run_id;
        match text(&value["type"]) {
            "message" => {
                let user = value["role"] == "user";
                if user && !replay {
                    return;
                }
                let body = value["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                self.message(
                    run,
                    if user {
                        "user_message_chunk"
                    } else {
                        "agent_message_chunk"
                    },
                    &body,
                    replay,
                    updates,
                );
            }
            "tool_call" => {
                let id = format!("{run}:{}", text(&value["call_id"]));
                let name = text(&value["name"]).rsplit('.').next().unwrap_or("unknown");
                let mut args: Value = serde_json::from_str(text(&value["arguments"]))
                    .unwrap_or_else(|_| json!({ "unparsed_arguments": value["arguments"] }));
                if !args.is_object() {
                    args = json!({ "arguments": args });
                }
                let title = args["command"]
                    .as_str()
                    .or_else(|| args["path"].as_str())
                    .unwrap_or(name);
                let kind = match name {
                    "read" => "read",
                    "write" | "edit" => "edit",
                    "bash" | "powershell" => "execute",
                    _ => "other",
                };
                let input = if kind == "other" {
                    json!({ "variant": "UseTool", "tool_name": value["name"], "tool_input": args })
                } else {
                    args.clone()
                };
                updates.push(json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": id,
                    "title": title,
                    "kind": kind,
                    "status": "in_progress",
                    "rawInput": input,
                    "content": [],
                }));
                self.tools.insert(id, (name.into(), args));
            }
            "tool_result" => {
                let id = format!("{run}:{}", text(&value["call_id"]));
                let (name, args) = self.tools.get(&id).cloned().unwrap_or_default();
                let result = &value["result"];
                let failed = !result["error"].is_null()
                    || result["exit_code"].as_i64().is_some_and(|code| code != 0);
                let mut body = text(&result["text"]).to_owned();
                if !result["error"].is_null() {
                    body.push_str(&format!("\n{}", result["error"]));
                }
                for artifact in result["artifacts"].as_array().into_iter().flatten() {
                    body.push_str(&format!(
                        "\nFull output: {} ({} bytes)",
                        text(&artifact["path"]),
                        artifact["bytes"]
                    ));
                }
                let mut content = vec![json!({
                    "type": "content",
                    "content": { "type": "text", "text": body },
                })];
                let details = &result["details"];
                let diffs: Vec<(&Value, u64)> = if let Some(edits) = details["edits"].as_array() {
                    edits
                        .iter()
                        .map(|edit| (&edit["diff"], edit["start_line"].as_u64().unwrap_or(1)))
                        .collect()
                } else {
                    vec![(details.get("diff").unwrap_or(details), 1)]
                };
                let mut offset = 0i64;
                for (diff, line) in diffs {
                    if let (Some(before), Some(after)) =
                        (diff["before"].as_str(), diff["after"].as_str())
                    {
                        content.push(json!({
                            "type": "diff",
                            "path": text(&args["path"]),
                            "oldText": before,
                            "newText": after,
                            "_meta": {
                                "old_line": line,
                                "new_line": line.saturating_add_signed(offset),
                            },
                        }));
                        offset += after.bytes().filter(|b| *b == b'\n').count() as i64
                            - before.bytes().filter(|b| *b == b'\n').count() as i64;
                    }
                }
                let raw = if matches!(name.as_str(), "bash" | "powershell") {
                    json!({
                        "type": "Bash",
                        "output": body.as_bytes(),
                        "exit_code":
                            result["exit_code"].as_i64().unwrap_or(i64::from(failed)),
                        "command": args["command"],
                        "truncated": result["truncated"] == true,
                        "signal": null,
                        "timed_out": false,
                        "description": null,
                        "current_dir": "",
                        "output_file": "",
                        "total_bytes": body.len(),
                    })
                } else {
                    result.clone()
                };
                updates.push(json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": id,
                    "status": if failed {
                            "failed"
                        } else {
                            "completed"
                        },
                    "content": content,
                    "rawOutput": raw,
                }));
            }
            _ if record.kind == "model_selection"
                && value["target"]["provider"].is_string()
                && value["target"]["model"].is_string() =>
            {
                updates.push(json!({ "_eden_model": value["target"] }));
            }
            _ if record.kind == "user_shell" => {
                let result = &value["result"];
                let body = text(&result["text"]);
                let failed = !result["error"].is_null()
                    || result["exit_code"].as_i64().is_some_and(|code| code != 0);
                updates.push(json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": format!("user-shell:{run}"),
                    "title": value["command"],
                    "kind": "execute",
                    "status": if failed {
                            "failed"
                        } else {
                            "completed"
                        },
                    "_meta": { "bash_mode": true },
                    "rawInput": { "command": value["command"] },
                    "rawOutput": {
                        "type": "Bash",
                        "output": body.as_bytes(),
                        "exit_code":
                            result["exit_code"].as_i64().unwrap_or(i64::from(failed)),
                        "command": value["command"],
                        "truncated": result["truncated"] == true,
                        "signal": null,
                        "timed_out": false,
                        "description": null,
                        "current_dir": "",
                        "output_file": "",
                        "total_bytes": body.len(),
                    },
                }));
            }
            _ if record.kind == "model_attempt" => self.message(
                run,
                "agent_message_chunk",
                text(&value["text"]),
                replay,
                updates,
            ),
            _ if record.kind == "terminal" => {
                self.closed_runs.insert(run);
                let status = text(&value["outcome"]["status"]);
                if matches!(status, "failed" | "cancelled") {
                    updates.push(chunk("agent_message_chunk", &format!("\n\nRun {status}.")));
                }
                if status == "failed"
                    && let Ok(eden_protocol::Outcome::Failed(error)) =
                        serde_json::from_value::<eden_protocol::Outcome>(value["outcome"].clone())
                {
                    updates.push(chunk("agent_message_chunk", &format!("\n{error}")));
                }
                if value["cleanup_errors"]
                    .as_array()
                    .is_some_and(|errors| !errors.is_empty())
                {
                    updates.push(chunk(
                        "agent_message_chunk",
                        &format!("\nCleanup: {}", value["cleanup_errors"]),
                    ));
                }
                self.streamed.retain(|(owner, _), _| *owner != run);
            }
            _ => {}
        }
        for update in &mut updates[start..] {
            update["_eden_run"] = json!(run);
            update["_eden_attempt"] = value["attempt_id"].clone();
        }
    }

    fn message(
        &mut self,
        run: u64,
        kind: &str,
        body: &str,
        replay: bool,
        updates: &mut Vec<Value>,
    ) {
        let prefix = self
            .streamed
            .remove(&(run, kind.into()))
            .unwrap_or_default();
        let body = if replay {
            body
        } else {
            body.strip_prefix(&prefix).unwrap_or(body)
        };
        if !body.is_empty() {
            updates.push(chunk(kind, body));
        }
    }
}

pub(crate) fn usage_text(records: &[eden_protocol::coding::Record]) -> String {
    let Some(value) = records.iter().rev().find_map(|record| {
        record
            .payload
            .get("usage")
            .filter(|value| value.is_object())
    }) else {
        return "Token usage has not been reported.".into();
    };
    let counts = value.get("normalized").unwrap_or(value);
    let count = |key: &str, fallback: &str| {
        counts[key]
            .as_u64()
            .or_else(|| counts[fallback].as_u64())
            .map_or_else(|| "?".into(), |value| value.to_string())
    };
    format!(
        "Latest reported request\nInput: {}{}\nOutput: {}\nCache read: {}\nCache write: \
         {}\nReasoning: {}",
        count("input_tokens", "prompt_tokens"),
        if value.get("normalized").is_some() {
            " uncached"
        } else {
            ""
        },
        count("output_tokens", "completion_tokens"),
        count("cache_read_tokens", "cache_read_tokens"),
        count("cache_write_tokens", "cache_write_tokens"),
        count("reasoning_tokens", "reasoning_tokens")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(
        sequence: u64,
        run: u64,
        kind: &str,
        payload: Value,
    ) -> eden_protocol::coding::Record {
        serde_json::from_value(json!({
            "schema_version": 1,
            "session_id": 3,
            "sequence": sequence,
            "run_id": run,
            "kind": kind,
            "payload": payload,
        }))
        .unwrap()
    }
    fn snapshot(events: Value, active: Option<u64>) -> Snapshot {
        serde_json::from_value(json!({
            "presentation": {
                "version": 1,
                "session_id": 3,
                "sequence": 0,
                "views": [],
                "activity": [],
                "pending_interactions": [],
            },
            "state": { "session_id": 3, "closed": false, "active_run": active },
            "history": [],
            "events": events,
        }))
        .unwrap()
    }
    #[test]
    fn unknown_tools_keep_their_supplied_parameters() {
        let mut projection = Projection::default();
        let mut updates = vec![];
        projection.record(
            &record(
                1,
                7,
                "tool_call",
                json!({
                    "type": "tool_call",
                    "call_id": "custom",
                    "name": "plugin.invoke",
                    "arguments":
                        "{\"tool_name\":\"inner\",\"path\":\"outer\",\"file_path\":\"inner-file\"}",
                }),
            ),
            false,
            &mut updates,
        );
        assert_eq!(updates[0]["rawInput"]["tool_input"]["tool_name"], "inner");
        assert_eq!(
            updates[0]["rawInput"]["tool_input"]["file_path"],
            "inner-file"
        );
    }
    #[test]
    fn failed_terminal_keeps_the_provider_diagnostic() {
        let mut projection = Projection::default();
        let mut updates = vec![];
        let terminal = eden_protocol::Terminal::failed(eden_protocol::Fault::new(
            "NoCredentials",
            "provider",
            "Configure an API key",
        ));
        projection.record(
            &record(1, 7, "terminal", serde_json::to_value(terminal).unwrap()),
            false,
            &mut updates,
        );
        assert!(
            updates
                .iter()
                .any(|update| text(&update["content"]["text"]).contains("Configure an API key"))
        );
    }
    #[test]
    fn usage_does_not_turn_unknown_counters_into_zero() {
        let body = usage_text(&[record(
            1,
            7,
            "model_response",
            json!({ "usage": { "normalized": { "input_tokens": 12, "output_tokens": null } } }),
        )]);
        assert!(body.contains("Input: 12 uncached"));
        assert!(body.contains("Output: ?"));
    }
    #[test]
    fn late_delta_from_finished_attempt_is_not_presented() {
        let mut projection = Projection::default();
        let frame = snapshot(
            json!([
                {
                    "sequence": 1,
                    "session_id": 3,
                    "run_id": 7,
                    "kind": "model_attempt_finished",
                    "payload": { "attempt_id": "old", "status": "cancelled" },
                },
                {
                    "sequence": 2,
                    "session_id": 3,
                    "run_id": 7,
                    "kind": "model_text_delta",
                    "payload": { "attempt_id": "old", "delta": "late" },
                },
            ]),
            None,
        );
        assert!(projection.apply(&frame, false).is_empty());
    }
    #[test]
    fn attaching_during_a_run_reconstructs_uncommitted_text() {
        let mut projection = Projection::default();
        let frame = snapshot(
            json!([
                {
                    "sequence": 1,
                    "session_id": 3,
                    "run_id": 7,
                    "kind": "model_attempt_started",
                    "payload": { "attempt_id": "live" },
                },
                {
                    "sequence": 2,
                    "session_id": 3,
                    "run_id": 7,
                    "kind": "model_text_delta",
                    "payload": { "attempt_id": "live", "delta": "buffered text" },
                },
            ]),
            Some(7),
        );
        let updates = projection.apply(&frame, true);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["content"]["text"], "buffered text");
        assert_eq!(updates[0]["_eden_run"], 7);
        assert_eq!(updates[0]["_eden_attempt"], "live");
        assert!(projection.apply(&frame, false).is_empty());
    }
    #[test]
    fn durable_commit_does_not_repeat_streamed_prefix() {
        let mut projection = Projection::default();
        projection
            .streamed
            .insert((7, "agent_message_chunk".into()), "Hello ".into());
        let mut updates = vec![];
        projection.record(
            &record(
                1,
                7,
                "message",
                json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "text": "Hello world" }],
                }),
            ),
            false,
            &mut updates,
        );
        assert_eq!(updates[0]["content"]["text"], "world");
    }
    #[test]
    fn cancelled_attempt_text_is_replayed_once() {
        let mut projection = Projection::default();
        let mut updates = vec![];
        let record = record(
            1,
            7,
            "model_attempt",
            json!({ "text": "partial answer", "status": "cancelled" }),
        );
        projection.record(&record, true, &mut updates);
        projection.record(&record, true, &mut updates);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["content"]["text"], "partial answer");
        assert_eq!(updates[0]["_eden_run"], 7);
    }
}
