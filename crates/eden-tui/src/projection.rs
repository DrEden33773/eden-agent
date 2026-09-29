//! Project authoritative records and attempt-scoped observations without inventing tool success.
use crate::{
    model::{Message, Role},
    summary::RunStats,
};
use eden_protocol::{
    Event, Outcome, Terminal,
    coding::{Artifact, Block, Item, Record, ToolResult},
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

fn blocks(content: &[Block]) -> String {
    content
        .iter()
        .map(|block| match block {
            Block::Text { text } => text.clone(),
            Block::Image { media_type, .. } => format!("[Image: {media_type}]"),
            Block::File {
                name, media_type, ..
            } => format!("[Attachment: {name} · {media_type}]"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn thinking(value: &Value) -> String {
    let raw = value.get("state").unwrap_or(value);
    let mut text = raw["thinking"]
        .as_str()
        .or_else(|| raw["reasoning_content"].as_str())
        .unwrap_or("")
        .to_owned();
    for key in ["summary", "reasoning_details"] {
        for part in raw[key].as_array().into_iter().flatten() {
            if let Some(part) = part["text"].as_str().or_else(|| part["summary"].as_str()) {
                text.push_str(part);
            }
        }
    }
    text
}
fn result_body(result: &ToolResult) -> String {
    let mut text = result.text.clone();
    if !result.content.is_empty() {
        text.push_str(&format!("\n{}", blocks(&result.content)));
    }
    if let Some(error) = &result.error {
        text.push_str(&format!("\n{}: {}", error.code, error.message));
    }
    if let Some(code) = result.exit_code {
        text.push_str(&format!("\nexit {code}"));
    }
    if result.truncated {
        text.push_str("\nPreview truncated; complete output is retained in the listed artifacts.");
    }
    for artifact in &result.artifacts {
        text.push_str(&format!(
            "\n{}: {} ({} bytes, {})",
            artifact.name, artifact.path, artifact.bytes, artifact.media_type
        ));
    }
    text
}
fn failed(result: &ToolResult) -> bool {
    result.error.is_some() || result.exit_code.is_some_and(|code| code != 0)
}
fn apply_diff(message: &mut Message, details: &Value) {
    let direct = details.get("diff").unwrap_or(details);
    let diffs: Vec<_> = if direct["before"].is_string() && direct["after"].is_string() {
        vec![direct]
    } else {
        details["edits"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|edit| &edit["diff"])
            .filter(|diff| diff["before"].is_string() && diff["after"].is_string())
            .collect()
    };
    if diffs.is_empty() {
        return;
    }
    if diffs.len() == 1 {
        message.before = diffs[0]["before"].as_str().map(str::to_owned);
        message.after = diffs[0]["after"].as_str().map(str::to_owned);
    } else {
        // Keep each disjoint edit visible without pretending omitted file ranges are adjacent.
        for (index, diff) in diffs.iter().enumerate() {
            message
                .body
                .push_str(&format!("\n\nChange {}\n```diff\n", index + 1));
            for line in diff["before"].as_str().unwrap_or("").lines() {
                message.body.push_str(&format!("-{line}\n"));
            }
            for line in diff["after"].as_str().unwrap_or("").lines() {
                message.body.push_str(&format!("+{line}\n"));
            }
            message.body.push_str("```\n");
        }
    }
    if diffs.iter().any(|diff| diff["complete"] == false) {
        message
            .body
            .push_str("\nDiff preview only; before/after artifacts retain the complete contents.");
    }
}
fn terminal_notice(record: &Record, terminal: &Terminal) -> Option<Message> {
    let text = match &terminal.outcome {
        Outcome::Failed(error) => format!("{}: {}", error.code, error.message),
        Outcome::Cancelled => "Run cancelled; external effects may already have occurred.".into(),
        Outcome::Completed(_) if terminal.cleanup_errors.is_empty() => return None,
        Outcome::Completed(_) => "Run cleanup failed.".into(),
    };
    let mut message = Message::new(record.sequence, Role::Notice, "Run result", text);
    message.failed = !matches!(terminal.outcome, Outcome::Cancelled);
    for error in &terminal.cleanup_errors {
        message
            .body
            .push_str(&format!("\n{}: {}", error.code, error.message));
    }
    Some(message)
}
pub fn history(records: &[Record]) -> Vec<Message> {
    let mut messages: Vec<Message> = Vec::new();
    let mut calls = BTreeMap::new();
    let mut stats: BTreeMap<u64, RunStats> = BTreeMap::new();
    let mut delivered = BTreeSet::new();
    for record in records {
        match serde_json::from_value::<Item>(record.payload.clone()) {
            Ok(Item::Message { role, content }) => {
                let (kind, title) = match role.as_str() {
                    "user" => (Role::User, "You"),
                    "assistant" => (Role::Assistant, "Eden"),
                    other => (Role::Notice, other),
                };
                let mut message = Message::new(record.sequence, kind, title, blocks(&content));
                for reference in record.payload["references"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    let source = &reference["source"];
                    message.body.push_str(&format!(
                        "\n[Fixed session reference: {} / {} @ {} · source system {}]",
                        source["label"].as_str().unwrap_or("unknown"),
                        source["branch"].as_str().unwrap_or("unknown"),
                        source["head"],
                        if reference["source_system"].is_null() {
                            "unknown"
                        } else {
                            "captured"
                        }
                    ));
                }
                messages.push(message);
            }
            Ok(Item::ToolCall {
                call_id,
                name,
                arguments,
            }) => {
                let args: Value = serde_json::from_str(&arguments).unwrap_or(Value::Null);
                let path = args["path"].as_str();
                stats
                    .entry(record.run_id)
                    .or_default()
                    .record_start(&name, path);
                let title = path.map_or_else(|| name.clone(), |path| format!("{name} {path}"));
                let mut message = Message::new(
                    record.sequence,
                    Role::Tool,
                    title,
                    format!("Input\n{arguments}\n\nOutput\nWaiting for result"),
                );
                message.pending = true;
                calls.insert((record.run_id, call_id), messages.len());
                messages.push(message);
            }
            Ok(Item::ToolResult { call_id, result }) => {
                let index = calls.get(&(record.run_id, call_id.clone())).copied();
                let mut message = index
                    .map(|index| messages[index].clone())
                    .unwrap_or_else(|| {
                        Message::new(
                            record.sequence,
                            Role::Tool,
                            format!("Tool {call_id} (name unavailable)"),
                            "Input unavailable in this history",
                        )
                    });
                message.pending = false;
                message.failed = failed(&result);
                stats
                    .entry(record.run_id)
                    .or_default()
                    .record_result(message.failed);
                message.body = message
                    .body
                    .split("\n\nOutput\n")
                    .next()
                    .unwrap_or("")
                    .to_owned();
                message
                    .body
                    .push_str(&format!("\n\nOutput\n{}", result_body(&result)));
                apply_diff(&mut message, &result.details);
                message.revision = record.sequence;
                if let Some(index) = index {
                    messages[index] = message;
                } else {
                    messages.push(message);
                }
            }
            Ok(Item::ProviderState { value, .. }) => {
                let text = thinking(&value);
                if !text.is_empty() {
                    messages.push(Message::new(
                        record.sequence,
                        Role::Thinking,
                        "Thinking",
                        text,
                    ));
                }
            }
            Err(_) => match record.kind.as_str() {
                "queue_delivered" if delivered.insert(record.payload["id"].to_string()) => {
                    if let Ok(content) =
                        serde_json::from_value::<Vec<Block>>(record.payload["content"].clone())
                    {
                        messages.push(Message::new(
                            record.sequence,
                            Role::User,
                            "You · queued",
                            blocks(&content),
                        ));
                    }
                }
                "context_edit" => {
                    messages.push(Message::new(
                        record.sequence,
                        Role::Notice,
                        "Context edited",
                        format!(
                            "Edit #{} · {} · {} · original transcript retained · F7 inspects \
                             effective input",
                            record.sequence,
                            record.payload["source"]
                                .as_str()
                                .unwrap_or("unknown source"),
                            record.payload["scope"].as_str().unwrap_or("unknown scope")
                        ),
                    ));
                }
                "model_attempt" => {
                    let status = record.payload["status"].as_str().unwrap_or("interrupted");
                    let mut message = Message::new(
                        record.sequence,
                        Role::Assistant,
                        format!("Eden · {status} attempt"),
                        record.payload["text"].as_str().unwrap_or(""),
                    );
                    message.failed = status != "completed";
                    if let Some(error) = record.payload["error"]["message"].as_str() {
                        message.body.push_str(&format!("\n{error}"));
                    }
                    messages.push(message);
                }
                "user_shell" => {
                    let shell = record.payload["shell"].as_str().unwrap_or("shell");
                    let command = record.payload["command"].as_str().unwrap_or("");
                    let mut message = Message::new(
                        record.sequence,
                        Role::Tool,
                        format!("{shell} · user command"),
                        format!("Input\n{command}\n\nOutput\n"),
                    );
                    match serde_json::from_value::<ToolResult>(record.payload["result"].clone()) {
                        Ok(result) => {
                            message.failed = failed(&result);
                            message.body.push_str(&result_body(&result));
                        }
                        Err(_) => {
                            message.failed = true;
                            message.body.push_str("Result could not be decoded.");
                        }
                    }
                    if record.payload["exclude_from_context"] == true {
                        message.body.push_str("\nExcluded from model context.");
                    }
                    messages.push(message);
                }
                "terminal" => {
                    if let Ok(terminal) = serde_json::from_value::<Terminal>(record.payload.clone())
                    {
                        for ((run, _), index) in &calls {
                            if *run == record.run_id && messages[*index].pending {
                                messages[*index].pending = false;
                                messages[*index].failed = true;
                                messages[*index]
                                    .body
                                    .push_str("\nRun settled without a recorded tool result.");
                                messages[*index].revision = record.sequence;
                            }
                        }
                        if let Some(message) = terminal_notice(record, &terminal) {
                            messages.push(message);
                        } else if let Some(summary) =
                            stats.get(&record.run_id).and_then(|stats| stats.finish(0))
                        {
                            messages.push(Message::run_summary(record.sequence, summary));
                        } else if let Outcome::Completed(value) = &terminal.outcome
                            && !value.is_null()
                            && !records.iter().any(|prior| {
                                prior.run_id == record.run_id
                                    && prior.sequence < record.sequence
                                    && (prior.kind == "user_shell"
                                        || (prior.payload["type"] == "message"
                                            && prior.payload["role"] == "assistant"))
                            })
                        {
                            messages.push(Message::new(
                                record.sequence,
                                Role::Notice,
                                "Run result",
                                value.as_str().map(str::to_owned).unwrap_or_else(|| {
                                    serde_json::to_string_pretty(value).unwrap_or_default()
                                }),
                            ));
                        }
                    }
                }
                _ => {}
            },
        }
    }
    messages
}

#[derive(Default)]
struct Attempt {
    request: String,
    status: String,
    committed: bool,
    streams: BTreeMap<(String, String), Message>,
}
fn stream_identity(payload: &Value) -> String {
    for key in [
        "index",
        "output_index",
        "contentIndex",
        "call_id",
        "item_id",
    ] {
        if let Some(value) = payload.get(key).filter(|value| !value.is_null()) {
            return format!("{key}:{value}");
        }
    }
    "message".into()
}
pub fn streaming(events: &[Event], active: Option<u64>) -> Vec<Message> {
    project_streams(events, active, &[])
}
pub fn streaming_with_history(
    events: &[Event],
    active: Option<u64>,
    records: &[Record],
) -> Vec<Message> {
    if records.is_empty() {
        streaming(events, active)
    } else {
        project_streams(events, active, records)
    }
}
fn project_streams(events: &[Event], active: Option<u64>, records: &[Record]) -> Vec<Message> {
    let Some(run) = active else {
        return vec![];
    };
    let durable_attempts: BTreeSet<_> = records
        .iter()
        .filter(|record| record.run_id == run && record.kind == "model_attempt")
        .filter_map(|record| record.payload["attempt_id"].as_str())
        .collect();
    let durable_requests: BTreeSet<_> = records
        .iter()
        .filter(|record| record.run_id == run && record.kind == "model_response")
        .filter_map(|record| record.payload["request_id"].as_str())
        .collect();
    // Sequence is authoritative on reconnect; a repeated retained window must not duplicate deltas.
    let ordered: BTreeMap<_, _> = events
        .iter()
        .filter(|event| event.run_id == run)
        .map(|event| (event.sequence, event))
        .collect();
    let mut attempts: BTreeMap<String, Attempt> = BTreeMap::new();
    let mut request = String::new();
    for event in ordered.into_values() {
        if event.kind == "model_request" {
            request = event.payload["request_id"].as_str().unwrap_or("").into();
        }
        if event.kind == "committed" && event.payload["kind"] == "model_response" {
            for attempt in attempts
                .values_mut()
                .filter(|attempt| attempt.request == request && attempt.status == "completed")
            {
                attempt.committed = true;
            }
        }
        let Some(id) = event.payload["attempt_id"].as_str() else {
            continue;
        };
        let attempt = attempts.entry(id.into()).or_insert_with(|| Attempt {
            request: request.clone(),
            ..Default::default()
        });
        match event.kind.as_str() {
            "model_attempt_started" => {
                attempt.request = request.clone();
            }
            "model_attempt_finished" => {
                attempt.status = event.payload["status"]
                    .as_str()
                    .unwrap_or("interrupted")
                    .into();
                for message in attempt.streams.values_mut() {
                    message.pending = false;
                    message.failed = attempt.status != "completed";
                    if message.failed {
                        message.title = format!("{} · {} attempt", message.title, attempt.status);
                    }
                }
            }
            "model_text_delta" | "model_reasoning_delta" | "model_tool_delta"
                if attempt.status.is_empty() =>
            {
                let Some(delta) = event.payload["delta"].as_str() else {
                    continue;
                };
                let (role, title) = match event.kind.as_str() {
                    "model_tool_delta" => (Role::Tool, "Tool arguments · incomplete"),
                    "model_reasoning_delta" => (Role::Thinking, "Thinking"),
                    _ => (Role::Assistant, "Eden"),
                };
                let message = attempt
                    .streams
                    .entry((event.kind.clone(), stream_identity(&event.payload)))
                    .or_insert_with(|| Message::new(u64::MAX - event.sequence, role, title, ""));
                message.body.push_str(delta);
                message.revision = event.sequence;
                message.pending = true;
            }
            _ => {}
        }
    }
    let mut messages: Vec<_> = attempts
        .into_iter()
        .filter(|(id, attempt)| {
            !attempt.committed
                && !durable_attempts.contains(id.as_str())
                && !(attempt.status == "completed"
                    && durable_requests.contains(attempt.request.as_str()))
        })
        .flat_map(|(_, attempt)| {
            attempt
                .streams
                .into_values()
                .filter(move |message| message.role != Role::Tool || attempt.status.is_empty())
        })
        .collect();
    messages.sort_by_key(|message| std::cmp::Reverse(message.id));
    messages
}

/// Shell bytes are joined before UTF-8 display conversion so a split multibyte character survives.
pub fn shell_streaming(events: &[Event], runs: &[u64]) -> Vec<Message> {
    let ordered: BTreeMap<_, _> = events
        .iter()
        .filter(|event| runs.contains(&event.run_id) && event.kind == "user_shell_output")
        .map(|event| (event.sequence, event))
        .collect();
    let mut streams: BTreeMap<(u64, String), (u64, u64, Vec<u8>)> = BTreeMap::new();
    for event in ordered.into_values() {
        let stream = event.payload["stream"].as_str().unwrap_or("output");
        let entry = streams.entry((event.run_id, stream.into())).or_insert((
            event.sequence,
            event.sequence,
            vec![],
        ));
        if let Ok(bytes) = serde_json::from_value::<Vec<u8>>(event.payload["bytes"].clone()) {
            entry.2.extend(bytes);
            entry.1 = event.sequence;
        }
    }
    streams
        .into_iter()
        .map(|((run, stream), (first, last, bytes))| {
            let mut message = Message::new(
                u64::MAX - first,
                Role::Tool,
                format!("Shell {run} · {stream}"),
                String::from_utf8_lossy(&bytes),
            );
            message.revision = last;
            message.pending = true;
            message
        })
        .collect()
}
/// Return immutable output references for a transcript tool, using its original intention identity.
pub fn artifacts(records: &[Record], message_id: u64) -> Vec<Artifact> {
    let Some(record) = records.iter().find(|record| record.sequence == message_id) else {
        return vec![];
    };
    let result = if record.kind == "user_shell" || record.payload["type"] == "tool_result" {
        record.payload["result"].clone()
    } else if record.payload["type"] == "tool_call" {
        records
            .iter()
            .find(|candidate| {
                candidate.run_id == record.run_id
                    && candidate.payload["type"] == "tool_result"
                    && candidate.payload["call_id"] == record.payload["call_id"]
            })
            .map(|candidate| candidate.payload["result"].clone())
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    serde_json::from_value::<ToolResult>(result)
        .map(|result| result.artifacts)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn record(sequence: u64, kind: &str, payload: Value) -> Record {
        Record {
            schema_version: 2,
            session_id: 1,
            run_id: 7,
            sequence,
            parent_id: sequence.checked_sub(1).filter(|id| *id > 0),
            branch: "main".into(),
            kind: kind.into(),
            payload,
        }
    }
    fn output(text: &str) -> ToolResult {
        ToolResult {
            text: text.into(),
            content: vec![],
            details: Value::Null,
            artifacts: vec![],
            exit_code: None,
            error: None,
            truncated: false,
        }
    }
    #[test]
    fn retries_late_deltas_and_replayed_windows_do_not_mix_attempts() {
        let events = vec![
            event(1, "model_request", json!({ "request_id": "request" })),
            event(2, "model_attempt_started", json!({ "attempt_id": "a" })),
            event(
                3,
                "model_text_delta",
                json!({ "attempt_id": "a", "delta": "old" }),
            ),
            event(
                4,
                "model_tool_delta",
                json!({ "attempt_id": "a", "index": 0, "delta": "unsafe" }),
            ),
            event(
                5,
                "model_attempt_finished",
                json!({ "attempt_id": "a", "status": "cancelled" }),
            ),
            event(6, "model_attempt_started", json!({ "attempt_id": "b" })),
            event(
                7,
                "model_text_delta",
                json!({ "attempt_id": "b", "delta": "new" }),
            ),
            event(
                8,
                "model_text_delta",
                json!({ "attempt_id": "a", "delta": "late" }),
            ),
            event(
                9,
                "committed",
                json!({ "kind": "tool_result", "sequence": 20 }),
            ),
        ];
        let messages = streaming(&[events.clone(), events].concat(), Some(7));
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].body, "old");
        assert!(messages[0].failed && !messages[0].pending);
        assert_eq!(messages[1].body, "new");
        assert!(messages[1].pending);
    }
    #[test]
    fn parallel_tool_arguments_keep_string_and_index_identities() {
        let messages = streaming(
            &[
                event(
                    1,
                    "model_tool_delta",
                    json!({ "attempt_id": "a", "call_id": "first", "delta": "one" }),
                ),
                event(
                    2,
                    "model_tool_delta",
                    json!({ "attempt_id": "a", "call_id": "second", "delta": "two" }),
                ),
                event(
                    3,
                    "model_tool_delta",
                    json!({ "attempt_id": "a", "call_id": "first", "delta": "!" }),
                ),
                event(
                    4,
                    "model_tool_delta",
                    json!({ "attempt_id": "a", "index": 0, "delta": "index0" }),
                ),
                event(
                    5,
                    "model_tool_delta",
                    json!({ "attempt_id": "a", "index": 1, "delta": "index1" }),
                ),
            ],
            Some(7),
        );
        assert_eq!(
            messages
                .iter()
                .map(|message| message.body.as_str())
                .collect::<Vec<_>>(),
            ["one!", "two", "index0", "index1"]
        );
    }
    #[test]
    fn committed_response_replaces_only_its_successful_attempt() {
        let events = vec![
            event(1, "model_request", json!({ "request_id": "request" })),
            event(
                2,
                "model_text_delta",
                json!({ "attempt_id": "a", "delta": "answer" }),
            ),
            event(
                3,
                "model_attempt_finished",
                json!({ "attempt_id": "a", "status": "completed" }),
            ),
        ];
        let records = vec![record(
            1,
            "model_response",
            json!({ "request_id": "request" }),
        )];
        assert!(streaming_with_history(&events, Some(7), &records).is_empty());
        assert_eq!(streaming(&events, Some(7)).len(), 1);
    }
    #[test]
    fn history_uses_confirmed_edit_diff_and_keeps_failed_unknown_tool() {
        let mut result = output("Applied 1 exact edit(s)");
        result.details = json!({ "edits": [{ "diff": { "before": "old", "after": "new" } }] });
        let mut failed_result = output("");
        failed_result.error = Some(eden_protocol::Fault::new(
            "UnknownTool",
            "tools",
            "unsupported tool",
        ));
        let messages = history(&[
            record(
                1,
                "tool_intent",
                json!(Item::ToolCall {
                    call_id: "edit".into(),
                    name: "edit".into(),
                    arguments: "{\"path\":\"x.rs\"}".into()
                }),
            ),
            record(
                2,
                "tool_result",
                json!(Item::ToolResult {
                    call_id: "edit".into(),
                    result
                }),
            ),
            record(
                3,
                "tool_intent",
                json!(Item::ToolCall {
                    call_id: "other".into(),
                    name: "future_tool".into(),
                    arguments: "{}".into()
                }),
            ),
            record(
                4,
                "tool_result",
                json!(Item::ToolResult {
                    call_id: "other".into(),
                    result: failed_result
                }),
            ),
        ]);
        assert_eq!(messages[0].before.as_deref(), Some("old"));
        assert_eq!(messages[0].after.as_deref(), Some("new"));
        assert!(messages[1].failed && !messages[1].pending);
        assert!(messages[1].body.contains("UnknownTool"));
    }
    #[test]
    fn user_shell_renders_results_and_exposes_original_artifacts() {
        let mut result = output("tail");
        result.exit_code = Some(1);
        result.truncated = true;
        result.artifacts.push(Artifact {
            name: "stdout".into(),
            path: "/retained/stdout.bin".into(),
            bytes: 90000,
            media_type: "application/octet-stream".into(),
        });
        let records = vec![record(
            1,
            "user_shell",
            json!({
                "command": "false",
                "shell": "bash",
                "exclude_from_context": true,
                "result": result,
            }),
        )];
        let messages = history(&records);
        assert!(messages[0].failed);
        assert!(messages[0].body.contains("stdout.bin"));
        assert!(messages[0].body.contains("Excluded from model context"));
        assert_eq!(artifacts(&records, messages[0].id)[0].bytes, 90000);
    }
    #[test]
    fn shell_chunks_preserve_split_utf8_and_do_not_duplicate_on_replay() {
        let events = vec![
            event(
                1,
                "user_shell_output",
                json!({ "stream": "stdout", "bytes": [228, 184] }),
            ),
            event(
                2,
                "user_shell_output",
                json!({ "stream": "stdout", "bytes": [173] }),
            ),
        ];
        assert_eq!(
            shell_streaming(&[events.clone(), events].concat(), &[7])[0].body,
            "中"
        );
    }
}
