//! Default sequential coding loop, context projection and durable input queue.
use eden_plugin_sdk::protocol::resources as r;
use eden_plugin_sdk::{
    CallContext, Package,
    protocol::{Descriptor, Fault, coding::*},
    serde_json::{self, Value, json},
};
use std::collections::BTreeSet;
use std::sync::Arc;

async fn append(cx: &CallContext, kind: &str, payload: Value) -> Result<StoreReply, Fault> {
    let receipt: StoreReply = cx
        .call(
            STORE,
            &StoreRequest::Append {
                run_id: cx.run_id(),
                kind: kind.into(),
                payload,
            },
        )
        .await?;
    cx.emit(
        "committed",
        json!({ "sequence": receipt.sequence, "kind": kind }),
    )?;
    Ok(receipt)
}
fn item_kind(item: &Item) -> &'static str {
    match item {
        Item::ToolCall { .. } => "tool_intent",
        Item::ToolResult { .. } => "tool_result",
        Item::Message { .. } => "message",
        Item::ProviderState { .. } => "provider_state",
    }
}
mod context;
mod queue;
mod settings;
use context::context;
#[cfg(test)]
use context::project_records;
#[cfg(test)]
use queue::pending;
use queue::queue;
use settings::Settings;
fn tools() -> Vec<ToolDefinition> {
    [
        (
            "powershell",
            "Run native PowerShell without profiles at the explicit cwd. Waits for process-tree \
             cleanup on completion, cancellation or optional timeout_seconds deadline. Returns \
             separate bounded stream tails and persistent full-output artifacts.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_seconds": {
                        "type": "number",
                        "exclusiveMinimum": 0,
                        "description":
                            "Optional positive deadline in seconds; no default timeout.",
                    },
                },
                "required": ["command"],
                "additionalProperties": false,
            }),
        ),
        (
            "ls",
            "List immediate directory entries in name order.",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "additionalProperties": false,
            }),
        ),
        (
            "skill",
            "Load an available skill on demand. Relative references resolve from its skill \
             directory.",
            json!({
                "type": "object",
                "properties": { "name": { "type": "string" }, "arguments": { "type": "string" } },
                "required": ["name"],
                "additionalProperties": false,
            }),
        ),
        (
            "read",
            "Read UTF-8 text or PNG/JPEG/GIF/WebP images. Text uses 1-based offset and line \
             limit; follow details.next_offset/next_byte_offset to continue.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "minimum": 1 },
                    "limit": { "type": "integer", "minimum": 1 },
                    "byte_offset": { "type": "integer", "minimum": 0 },
                },
                "required": ["path"],
                "additionalProperties": false,
            }),
        ),
        (
            "write",
            "Write a UTF-8 file, creating parent directories.",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" }, "content": { "type": "string" } },
                "required": ["path", "content"],
                "additionalProperties": false,
            }),
        ),
        (
            "edit",
            "Apply old_text/new_text or a batch of edits against the original file. All matches \
             must be unique and nonoverlapping. Strict mode normalizes CRLF; tolerant mode \
             additionally normalizes curly quotes, Unicode dashes and trailing spaces/tabs. \
             Preserves BOM and newline style.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_text": { "type": "string" },
                    "new_text": { "type": "string" },
                    "mode": { "type": "string", "enum": ["strict", "tolerant"] },
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": { "type": "string" },
                                "new_text": { "type": "string" },
                            },
                            "required": ["old_text", "new_text"],
                            "additionalProperties": false,
                        },
                    },
                },
                "required": ["path"],
                "oneOf": [
                    { "required": ["old_text", "new_text"], "not": { "required": ["edits"] } },
                    {
                        "required": ["edits"],
                        "not": {
                            "anyOf": [{ "required": ["old_text"] }, { "required": ["new_text"] }],
                        },
                    },
                ],
                "additionalProperties": false,
            }),
        ),
        (
            "bash",
            "Run bash in the session cwd. Returns separate bounded stream tails, persistent \
             full-output artifacts and the exit code after process-tree cleanup. An optional \
             timeout_seconds deadline stops this command; there is no default timeout.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_seconds": {
                        "type": "number",
                        "exclusiveMinimum": 0,
                        "description":
                            "Optional positive deadline in seconds; no default timeout.",
                    },
                },
                "required": ["command"],
                "additionalProperties": false,
            }),
        ),
    ]
    .into_iter()
    .filter(|(name, _, _)| matches!(*name, "read" | "write" | "edit" | "bash"))
    .map(|(name, description, parameters)| ToolDefinition {
        name: name.into(),
        description: description.into(),
        parameters,
    })
    .collect()
}

async fn model_limits(cx: &CallContext) -> Result<ModelLimits, Fault> {
    match cx.call(MODEL_INFO, &Value::Null).await {
        Ok(limits) => Ok(limits),
        Err(error)
            if error.code == "MissingDependency"
                && error.source == "router"
                && error.message == MODEL_INFO =>
        {
            Ok(ModelLimits::default())
        }
        Err(error) => Err(error),
    }
}
async fn provider_retry(
    cx: &CallContext,
    input: &ModelInput,
    settings: &Settings,
) -> Result<ModelReply, Fault> {
    for attempt in 0..=settings.max_retries {
        match cx.call(PROVIDER, input).await {
            Ok(reply) => return Ok(reply),
            Err(error) => {
                if error.code == "Cancelled" {
                    return Err(error);
                }
                append(
                    cx,
                    "model_error",
                    json!({ "attempt": attempt, "error": error }),
                )
                .await?;
                if error.code != "RetryableProviderFailure" || attempt == settings.max_retries {
                    return Err(error);
                }
                let delay_ms = settings
                    .base_delay_ms
                    .saturating_mul(2_u64.saturating_pow(attempt))
                    .max(error.retry_after_ms.unwrap_or(0));
                cx.emit(
                    "model_retry",
                    json!({ "attempt": attempt + 1, "delay_ms": delay_ms }),
                )?;
                let cancellation = cx.scope.cancellation();
                tokio::select! {
                    _ = cancellation.cancelled() => {
                        return Err(Fault::new("Cancelled", "coding-loop", "retry cancelled"));
                    },
                    _ = tokio::time::sleep(std::time::Duration::from_millis(
                        delay_ms
                    )) => {}
                }
            }
        }
    }
    unreachable!("bounded retries return above")
}
async fn optional_call<I: serde::Serialize, O: serde::de::DeserializeOwned>(
    cx: &CallContext,
    contract: &str,
    input: &I,
) -> Result<Option<O>, Fault> {
    match cx.call(contract, input).await {
        Ok(reply) => Ok(Some(reply)),
        Err(error)
            if error.code == "MissingDependency"
                && error.source == "router"
                && error.message == contract =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
async fn prepare_input(
    cx: &CallContext,
    content: Vec<Block>,
    resources: Option<&r::Snapshot>,
) -> Result<Vec<Block>, Fault> {
    let mut prepared = content;
    if let Some(snapshot) = resources {
        for block in &mut prepared {
            if let Block::Text { text } = block {
                let reply: r::ResourceReply = cx
                    .call(
                        r::SOURCE,
                        &r::ResourceRequest::Expand { text: text.clone() },
                    )
                    .await?;
                if reply.snapshot.revision != snapshot.revision {
                    return Err(Fault::new(
                        "ResourceFailure",
                        "input",
                        "resource revision changed during input preparation",
                    ));
                }
                if let Some(expanded) = reply.text {
                    *text = expanded;
                }
            }
        }
    }
    Ok(optional_call::<_, r::InputHook>(
        cx,
        r::BEFORE_INPUT,
        &r::InputHook {
            content: prepared.clone(),
            resource_revision: resources.map_or(0, |s| s.revision),
        },
    )
    .await?
    .map_or(prepared, |hook| hook.content))
}
async fn run(mut input: RunInput, cx: CallContext, settings: Settings) -> Result<String, Fault> {
    let resource_reply: Option<r::ResourceReply> =
        optional_call(&cx, r::SOURCE, &r::ResourceRequest::Snapshot).await?;
    let resources = resource_reply.map(|reply| reply.snapshot);
    let catalog: Option<r::Catalog> = optional_call(
        &cx,
        r::TOOL_CATALOG,
        &r::CatalogRequest {
            cwd: input.cwd.clone(),
        },
    )
    .await?;
    let selected_tools = catalog.map(|catalog| catalog.tools);
    if !input.resume {
        append(&cx, "submission", json!({ "content": input.content })).await?;
    }
    if let Some(snapshot) = &resources {
        append(&cx, "resource_snapshot", json!(snapshot)).await?;
    }
    if !input.resume {
        let original = input.content.clone();
        input.content = prepare_input(&cx, input.content, resources.as_ref()).await?;
        append(
            &cx,
            "input_prepared",
            json!({
                "original": original,
                "content": input.content,
                "resource_revision": resources.as_ref().map_or(0, |s| s.revision),
            }),
        )
        .await?;
    }
    if !input.resume {
        append(
            &cx,
            "message",
            json!(Item::Message {
                role: "user".into(),
                content: input.content
            }),
        )
        .await?;
    }
    let limits = match &input.target {
        Some(target) => target.limits.clone(),
        None => model_limits(&cx).await?,
    };
    let mut final_answer = None;
    loop {
        let steering: Vec<QueueEntry> = cx
            .call(
                QUEUE,
                &QueueRequest::Take {
                    kind: "steering".into(),
                },
            )
            .await?;
        if steering.is_empty()
            && let Some(answer) = final_answer.take()
        {
            let follow: Vec<QueueEntry> = cx
                .call(
                    QUEUE,
                    &QueueRequest::Take {
                        kind: "follow_up".into(),
                    },
                )
                .await?;
            if follow.is_empty() {
                let pending: Vec<QueueEntry> = cx.call(QUEUE, &QueueRequest::Inspect).await?;
                if pending.iter().any(|entry| entry.kind == "steering") {
                    final_answer = Some(answer);
                    continue;
                }
                return Ok(answer);
            }
        }
        let history: StoreReply = cx.call(STORE, &StoreRequest::Read).await?;
        let mut projected: ModelInput = cx
            .call(
                CONTEXT,
                &ContextInput {
                    target: input.target.clone(),
                    resources: resources.clone(),
                    tools: selected_tools.clone(),
                    action: String::new(),
                    records: history.records.clone(),
                    instructions: String::new(),
                    limits: limits.clone(),
                    cwd: input.cwd.clone(),
                    items: vec![],
                },
            )
            .await?;
        // Context may have committed a summary request and checkpoint. Allocate
        // the next identity from that resulting history, never the stale input.
        let history: StoreReply = cx.call(STORE, &StoreRequest::Read).await?;
        let request_id = format!("{}:{}", cx.run_id(), history.sequence + 1);
        let deliveries = queue::statuses(&history.records);
        let queue_ids: Vec<_> = deliveries
            .iter()
            .filter(|(_, (state, record))| {
                *state == "queue_delivered" && record.run_id == cx.run_id()
            })
            .map(|(id, _)| *id)
            .collect();
        append(
            &cx,
            "model_request",
            json!({ "request_id": request_id, "queue_ids": queue_ids, "target": input.target }),
        )
        .await?;
        cx.emit(
            "model_request",
            json!({ "request_id": request_id, "queue_ids": queue_ids, "target": input.target }),
        )?;
        let reply = match provider_retry(&cx, &projected, &settings).await {
            Ok(reply) => reply,
            Err(error)
                if settings.compaction_enabled
                    && matches!(error.code.as_str(), "ContextOverflow" | "RecoverableLength") =>
            {
                let history: StoreReply = cx.call(STORE, &StoreRequest::Read).await?;
                let compacted: ModelInput = cx
                    .call(
                        CONTEXT,
                        &ContextInput {
                            target: input.target.clone(),
                            resources: resources.clone(),
                            tools: selected_tools.clone(),
                            action: "compact".into(),
                            records: history.records,
                            instructions: String::new(),
                            limits: limits.clone(),
                            cwd: input.cwd.clone(),
                            items: vec![],
                        },
                    )
                    .await?;
                projected = compacted;
                cx.emit(
                    "model_request",
                    json!({
                        "request_id": request_id,
                        "queue_ids": queue_ids,
                        "target": input.target,
                    }),
                )?;
                provider_retry(&cx, &projected, &settings).await?
            }
            Err(error) => return Err(error),
        };
        if reply.items.is_empty() {
            return Err(Fault::new(
                "ProviderFailure",
                "coding-loop",
                "provider returned no output",
            ));
        }
        let mut known: BTreeSet<String> = history
            .records
            .iter()
            .filter_map(|r| {
                r.payload
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect();
        for item in &reply.items {
            if let Item::ToolCall { call_id, .. } = item
                && (call_id.is_empty() || !known.insert(call_id.clone()))
            {
                return Err(Fault::new(
                    "ProviderFailure",
                    "coding-loop",
                    "empty or duplicate tool call identity",
                ));
            }
        }
        let response_history: StoreReply = cx.call(STORE, &StoreRequest::Read).await?;
        // Consumption and every intention share one durable response transaction.
        // A cancellation can therefore never consume input without its model response.
        let mut drafts: Vec<_> = reply
            .items
            .iter()
            .map(|item| RecordDraft {
                kind: item_kind(item).into(),
                payload: json!(item),
            })
            .collect();
        drafts.push(RecordDraft {
            kind: "model_response".into(),
            payload: json!({
                "request_id": request_id,
                "usage": reply.usage,
                "usage_generation": context::generation(
                    &eden_plugin_sdk::protocol::history::active_path(&response_history.records)?
                ),
                "response_estimate": context::estimate(&projected.items)
                        + context::estimate(&reply.items)
                        + serde_json::to_string(&projected.tools)
                            .map_or(0, |s| s.chars().count() as u64 / 4 + 1),
            }),
        });
        for (state, record) in deliveries.values() {
            if *state == "queue_delivered" && record.run_id == cx.run_id() {
                let mut payload = record.payload.clone();
                payload["request_id"] = json!(request_id);
                drafts.push(RecordDraft {
                    kind: "queue_consumed".into(),
                    payload,
                });
            }
        }
        let committed_kinds: Vec<_> = drafts.iter().map(|draft| draft.kind.clone()).collect();
        let receipt: StoreReply = cx
            .call(
                STORE,
                &StoreRequest::AppendBatch {
                    run_id: cx.run_id(),
                    entries: drafts,
                },
            )
            .await?;
        let first_sequence = receipt.sequence - committed_kinds.len() as u64 + 1;
        for (index, kind) in committed_kinds.iter().enumerate() {
            cx.emit(
                "committed",
                json!({ "sequence": first_sequence + index as u64, "kind": kind }),
            )?;
        }
        for id in &queue_ids {
            cx.emit(
                "queue_consumed",
                json!({ "id": id, "request_id": request_id }),
            )?;
        }
        let usage_overflow = settings.compaction_enabled
            && limits.context_window > 0
            && context::usage_tokens(&reply.usage).is_some_and(|n| {
                n > limits
                    .context_window
                    .saturating_sub(settings.reserve_tokens)
            });
        cx.emit("model_usage", reply.usage)?;
        let mut called = false;
        let mut answer = String::new();
        for item in reply.items {
            match item {
                Item::ToolCall {
                    call_id,
                    name,
                    arguments,
                } => {
                    called = true;
                    let result = match serde_json_decode(&arguments) {
                        Ok(arguments) => {
                            async {
                                let original = ToolRequest {
                                    cwd: input.cwd.clone(),
                                    call_id: call_id.clone(),
                                    name,
                                    arguments,
                                };
                                let execution =
                                    optional_call::<_, ToolRequest>(&cx, r::BEFORE_TOOL, &original)
                                        .await?
                                        .unwrap_or_else(|| original.clone());
                                if execution.call_id != original.call_id
                                    || execution.cwd != original.cwd
                                {
                                    // A single literal makes rustfmt skip this enclosing tool closure.
                                    return Err(Fault::new(
                                        "InvalidInput",
                                        "before-tool",
                                        "hooks may change tool name and arguments, but not call \
                                         identity or cwd",
                                    ));
                                }
                                append(
                                    &cx,
                                    "tool_execution_intent",
                                    json!({ "original": original, "execution": execution }),
                                )
                                .await?;
                                cx.call::<_, ToolResult>(TOOL, &execution).await
                            }
                            .await
                        }
                        Err(error) => Err(error),
                    };
                    let result = match result {
                        Ok(value) => value,
                        Err(error)
                            if error.code == "Cancelled" || error.code == "CleanupFailure" =>
                        {
                            return Err(error);
                        }
                        Err(error) => ToolResult {
                            content: vec![],
                            details: serde_json::Value::Null,
                            artifacts: vec![],
                            text: error.to_string(),
                            exit_code: None,
                            truncated: false,
                            error: Some(error),
                        },
                    };
                    append(
                        &cx,
                        "tool_result",
                        json!(Item::ToolResult {
                            call_id: call_id.clone(),
                            result
                        }),
                    )
                    .await
                    .map_err(|error| {
                        Fault::new(
                            "PersistenceFailure",
                            "coding-loop",
                            format!(
                                "Tool call {call_id}: external side effects may already have \
                                 occurred; result commit failed: {error}",
                                error = error
                            ),
                        )
                    })?;
                }
                Item::Message { content, .. } => {
                    for block in content {
                        if let Block::Text { text } = block {
                            answer.push_str(&text);
                        }
                    }
                }
                _ => {}
            }
        }
        if usage_overflow {
            let history: StoreReply = cx.call(STORE, &StoreRequest::Read).await?;
            let _: ModelInput = cx
                .call(
                    CONTEXT,
                    &ContextInput {
                        target: input.target.clone(),
                        resources: resources.clone(),
                        tools: selected_tools.clone(),
                        action: "compact".into(),
                        records: history.records,
                        instructions: String::new(),
                        limits: limits.clone(),
                        cwd: input.cwd.clone(),
                        items: vec![],
                    },
                )
                .await?;
        }
        final_answer = if called { None } else { Some(answer) };
    }
}
fn serde_json_decode(text: &str) -> Result<Value, Fault> {
    eden_plugin_sdk::serde_json::from_str(text)
        .map_err(|e| Fault::new("ToolFailure", "tool-arguments", e.to_string()))
}
fn descriptor() -> Descriptor {
    Descriptor {
        package: "coding".into(),
        version: "0.1.0".into(),
        provides: vec![LOOP.into(), CONTEXT.into(), QUEUE.into()],
    }
}
fn create(config: Value) -> Result<Package, Fault> {
    let settings = Settings::parse(config)?;
    let loop_settings = settings.clone();
    let queue_lock = Arc::new(tokio::sync::Mutex::new(()));
    Ok(Package::new("coding")
        .service(LOOP, move |input, cx| run(input, cx, loop_settings.clone()))
        .service(CONTEXT, move |input, cx| {
            context(input, cx, settings.clone())
        })
        .service(QUEUE, move |request, cx| {
            let lock = queue_lock.clone();
            async move {
                let (sender, receiver) = tokio::sync::oneshot::channel();
                let scope = cx.scope.clone();
                // The transaction lock must outlive a cancelled root while its
                // already admitted storage bridge finishes committing.
                scope.spawn(async move {
                    let _guard = lock.lock().await;
                    let result = queue(request, cx).await;
                    if let Err(Err(error)) = sender.send(result)
                        && !["Cancelled", "Unavailable"].contains(&error.code.as_str())
                    {
                        return Err(error);
                    }
                    Ok(())
                })?;
                receiver
                    .await
                    .map_err(|_| Fault::new("Unavailable", "queue", "queue completion lost"))?
            }
        }))
}
eden_plugin_sdk::export_plugin!(descriptor, create);
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interrupted_intent_is_not_replayed_or_sent_as_pending_provider_call() {
        let records = vec![Record {
            parent_id: None,
            branch: "main".into(),
            schema_version: 1,
            session_id: 1,
            sequence: 1,
            run_id: 1,
            kind: "tool_intent".into(),
            payload: json!(Item::ToolCall {
                call_id: "c1".into(),
                name: "write".into(),
                arguments: "{}".into()
            }),
        }];
        let projected = project_records(&records).unwrap();
        let Item::Message { content, .. } = &projected[0] else {
            panic!("an interrupted intent must project as context, not as work");
        };
        let Block::Text { text } = &content[0] else {
            panic!("the reopened context must be text");
        };
        assert!(text.contains("effects are unknown"), "{text}");
        assert!(text.contains("It was not replayed."), "{text}");
    }
}

#[cfg(test)]
mod s2_tests {
    use super::*;
    fn record(sequence: u64, kind: &str, payload: Value) -> Record {
        Record {
            parent_id: sequence.checked_sub(1).filter(|n| *n > 0),
            branch: "main".into(),
            schema_version: 2,
            session_id: 1,
            sequence,
            run_id: 1,
            kind: kind.into(),
            payload,
        }
    }
    #[test]
    fn returned_delivery_is_pending_and_not_projected_twice() {
        let entry = json!({
            "id": 1,
            "kind": "steering",
            "branch": "main",
            "content": [{ "type": "text", "text": "correct this" }],
        });
        let records = vec![
            record(1, "queue_accepted", entry.clone()),
            record(2, "queue_delivered", entry.clone()),
            record(3, "queue_returned", entry.clone()),
            record(4, "queue_delivered", entry.clone()),
        ];
        assert_eq!(project_records(&records).unwrap().len(), 1);
        let returned = &records[..3];
        assert_eq!(pending(returned).unwrap().len(), 1);
        assert!(project_records(returned).unwrap().is_empty());
    }
    #[test]
    fn consumption_survives_restore_and_uses_stable_identity() {
        let entry = json!({ "id": 1, "kind": "follow_up", "branch": "main", "content": [] });
        let records = vec![
            record(1, "queue_accepted", entry.clone()),
            record(2, "queue_delivered", entry.clone()),
            record(3, "queue_consumed", entry),
        ];
        assert!(pending(&records).unwrap().is_empty());
    }
    #[test]
    fn compaction_replaces_old_projection_and_keeps_recent_tool_round() {
        let records = vec![
            record(
                1,
                "message",
                json!(Item::Message {
                    role: "user".into(),
                    content: vec![Block::Text {
                        text: "old task".into()
                    }]
                }),
            ),
            record(
                2,
                "message",
                json!(Item::Message {
                    role: "user".into(),
                    content: vec![Block::Text {
                        text: "recent task".into()
                    }]
                }),
            ),
            record(
                3,
                "compaction",
                json!({
                    "summary": "retained goal",
                    "first_kept": 2,
                    "uncertainties": [],
                    "read_files": [],
                    "modified_files": [],
                }),
            ),
        ];
        let items = project_records(&records).unwrap();
        let text = serde_json::to_string(&items).unwrap();
        assert!(!text.contains("old task"));
        assert!(text.contains("retained goal"));
        assert!(text.contains("recent task"));
    }
}
