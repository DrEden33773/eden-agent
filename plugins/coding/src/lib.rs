//! Default sequential coding loop, context projection and durable input queue.
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
        json!({"sequence":receipt.sequence,"kind":kind}),
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
fn project_records(records: &[Record]) -> Result<Vec<Item>, Fault> {
    let mut items = vec![];
    for record in records {
        match record.kind.as_str() {
            "message" | "tool_intent" | "tool_result" | "provider_state" => items.push(
                serde_json::from_value::<Item>(record.payload.clone())
                    .map_err(|e| Fault::new("PersistenceFailure", "context", e.to_string()))?,
            ),
            "queue_delivered" => {
                let entry: QueueEntry = serde_json::from_value(record.payload.clone())
                    .map_err(|e| Fault::new("PersistenceFailure", "queue", e.to_string()))?;
                items.push(Item::Message {
                    role: "user".into(),
                    content: entry.content,
                });
            }
            _ => {}
        }
    }
    let completed: BTreeSet<_> = items
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    Ok(items.into_iter().map(|item| match item {
        Item::ToolCall { ref call_id, .. } if !completed.contains(call_id) => Item::Message { role: "user".into(), content: vec![Block::Text { text: format!("Historical tool call {call_id} was interrupted; result and external effects are unknown. It was not replayed.") }] },
        other => other,
    }).collect())
}
fn tools() -> Vec<ToolDefinition> {
    [
        ("read", "Read a UTF-8 file. offset is a 1-based line; limit bounds lines.", json!({"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1}},"required":["path"],"additionalProperties":false})),
        ("write", "Write a UTF-8 file, creating parent directories.", json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"],"additionalProperties":false})),
        ("edit", "Replace exactly one occurrence of old_text with new_text. Fails without changing the file if absent or ambiguous.", json!({"type":"object","properties":{"path":{"type":"string"},"old_text":{"type":"string"},"new_text":{"type":"string"}},"required":["path","old_text","new_text"],"additionalProperties":false})),
        ("bash", "Run a bash command in the session cwd. Returns bounded output and the exit code.", json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"],"additionalProperties":false})),
    ].into_iter().map(|(name,description,parameters)| ToolDefinition { name:name.into(), description:description.into(), parameters }).collect()
}
async fn context(input: ContextInput, _: CallContext) -> Result<ModelInput, Fault> {
    let mut items = vec![Item::Message {
        role: "system".into(),
        content: vec![Block::Text {
            text: format!(
                "You are eden, a coding assistant. Work in {}. Use read, write, edit and bash to inspect, change and verify the project. Report observed results accurately. Tool failures are evidence to address; do not claim unexecuted checks passed.",
                input.cwd
            ),
        }],
    }];
    items.extend(input.items);
    Ok(ModelInput {
        items,
        tools: tools(),
    })
}
async fn run(input: RunInput, cx: CallContext) -> Result<String, Fault> {
    append(
        &cx,
        "message",
        json!(Item::Message {
            role: "user".into(),
            content: input.content
        }),
    )
    .await?;
    loop {
        let _: Vec<QueueEntry> = cx
            .call(
                QUEUE,
                &QueueRequest::Take {
                    kind: "steering".into(),
                },
            )
            .await?;
        let history: StoreReply = cx.call(STORE, &StoreRequest::Read).await?;
        let projected: ModelInput = cx
            .call(
                CONTEXT,
                &ContextInput {
                    cwd: input.cwd.clone(),
                    items: project_records(&history.records)?,
                },
            )
            .await?;
        let reply: ModelReply = cx.call(PROVIDER, &projected).await?;
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
        // Every intention is committed before the first external side effect in the round.
        for item in &reply.items {
            append(&cx, item_kind(item), json!(item)).await?;
        }
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
                            cx.call::<_, ToolResult>(
                                TOOL,
                                &ToolRequest {
                                    cwd: input.cwd.clone(),
                                    call_id: call_id.clone(),
                                    name,
                                    arguments,
                                },
                            )
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
                            text: error.to_string(),
                            exit_code: None,
                            truncated: false,
                            error: Some(error),
                        },
                    };
                    append(
                        &cx,
                        "tool_result",
                        json!(Item::ToolResult { call_id: call_id.clone(), result }),
                    )
                    .await.map_err(|error| Fault::new("PersistenceFailure", "coding-loop", format!("Tool call {call_id}: external side effects may already have occurred; result commit failed: {error}")))?;
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
        if !called {
            let entries: Vec<QueueEntry> = cx
                .call(
                    QUEUE,
                    &QueueRequest::Take {
                        kind: "follow_up".into(),
                    },
                )
                .await?;
            if entries.is_empty() {
                let queued: Vec<QueueEntry> = cx.call(QUEUE, &QueueRequest::Inspect).await?;
                // An answer is also a turn boundary. Steering received during
                // its provider request must not require an unrelated follow-up
                // to keep the loop alive. Take it at the next loop boundary.
                if !queued.iter().any(|entry| entry.kind == "steering") {
                    return Ok(answer);
                }
            }
        }
    }
}
fn serde_json_decode(text: &str) -> Result<Value, Fault> {
    eden_plugin_sdk::serde_json::from_str(text)
        .map_err(|e| Fault::new("ToolFailure", "tool-arguments", e.to_string()))
}
fn pending(records: &[Record]) -> Result<Vec<QueueEntry>, Fault> {
    let delivered: BTreeSet<u64> = records
        .iter()
        .filter(|r| r.kind == "queue_delivered")
        .filter_map(|r| r.payload.get("id").and_then(Value::as_u64))
        .collect();
    records
        .iter()
        .filter(|r| r.kind == "queue_accepted")
        .map(|r| {
            eden_plugin_sdk::serde_json::from_value::<QueueEntry>(r.payload.clone())
                .map_err(|e| Fault::new("PersistenceFailure", "queue", e.to_string()))
        })
        .filter(|r| match r {
            Ok(entry) => !delivered.contains(&entry.id),
            Err(_) => true,
        })
        .collect()
}
async fn queue(request: QueueRequest, cx: CallContext) -> Result<Vec<QueueEntry>, Fault> {
    let history: StoreReply = cx.call(STORE, &StoreRequest::Read).await?;
    let entries = pending(&history.records)?;
    match request {
        QueueRequest::Inspect => Ok(entries),
        QueueRequest::Enqueue { kind, content } => {
            if !["steering", "follow_up"].contains(&kind.as_str()) {
                return Err(Fault::new("InvalidInput", "queue", "unknown queue kind"));
            }
            let entry = QueueEntry {
                id: history.sequence + 1,
                kind,
                content,
            };
            append(&cx, "queue_accepted", json!(entry)).await?;
            cx.emit("queue_accepted", json!(entry))?;
            Ok(vec![entry])
        }
        QueueRequest::Take { kind } => {
            let entry = entries.into_iter().find(|e| e.kind == kind);
            if let Some(entry) = entry {
                append(&cx, "queue_delivered", json!(entry)).await?;
                cx.emit("queue_delivered", json!(entry))?;
                Ok(vec![entry])
            } else {
                Ok(vec![])
            }
        }
    }
}
fn descriptor() -> Descriptor {
    Descriptor {
        package: "coding".into(),
        version: "0.1.0".into(),
        provides: vec![LOOP.into(), CONTEXT.into(), QUEUE.into()],
    }
}
fn create(_: Value) -> Result<Package, Fault> {
    let queue_lock = Arc::new(tokio::sync::Mutex::new(()));
    Ok(Package::new("coding")
        .service(LOOP, run)
        .service(CONTEXT, context)
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
        assert!(
            matches!(&projected[0],Item::Message { content,.. } if matches!(&content[0],Block::Text { text } if text.contains("unknown")))
        );
    }
}
