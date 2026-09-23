//! Independent reading projection; never loads a session's original business roles.
use base64::Engine;
use eden_plugin_sdk::{
    Package,
    protocol::{
        Descriptor, Fault,
        coding::{Block, Item, Record},
        delivery::*,
    },
    serde_json::{self, Value, json},
};
fn descriptor() -> Descriptor {
    Descriptor {
        package: "local-export".into(),
        version: "0.1.0".into(),
        provides: vec![EXPORTER.into()],
    }
}
fn create(_: Value) -> Result<Package, Fault> {
    Ok(
        Package::new("local-export").service(EXPORTER, |request: ExportRequest, _| async move {
            export(request)
        }),
    )
}
eden_plugin_sdk::export_plugin!(descriptor, create);
fn invalid(message: impl Into<String>) -> Fault {
    Fault::new("InvalidInput", "export", message)
}
fn blocks(content: Vec<Block>, selection: &Selection) -> Vec<Block> {
    content
        .into_iter()
        .filter(|b| selection.attachments || matches!(b, Block::Text { .. }))
        .collect()
}
fn path(records: &[Record], head: Option<u64>) -> Result<Vec<&Record>, Fault> {
    eden_plugin_sdk::protocol::history::validate_records(records)?;
    let mut head = head.or(eden_plugin_sdk::protocol::history::branch_state(records)?.0);
    let mut result = vec![];
    while let Some(id) = head {
        let record = records
            .iter()
            .find(|r| r.sequence == id && r.kind != "branch_selected")
            .ok_or_else(|| invalid("selected head is not a tree node"))?;
        result.push(record);
        head = record.parent_id;
    }
    result.reverse();
    Ok(result)
}
fn project(request: &ExportRequest) -> Result<(Vec<Value>, Vec<String>), Fault> {
    let mut entries = vec![];
    let mut warnings = vec![];
    let s = &request.selection;
    for record in path(&request.records, s.head)? {
        if !s.runs.is_empty() && !s.runs.contains(&record.run_id) {
            continue;
        }
        let content = match record.kind.as_str() {
            "message" | "tool_intent" | "tool_result" => {
                let item: Item = serde_json::from_value(record.payload.clone())
                    .map_err(|e| invalid(e.to_string()))?;
                match item {
                    Item::Message { role, content }
                        if s.messages && matches!(role.as_str(), "user" | "assistant") =>
                    {
                        json!({ "type": "message", "role": role, "content": blocks(content, s) })
                    }
                    Item::ToolCall {
                        call_id,
                        name,
                        arguments,
                    } if s.tools => {
                        json!({
                            "type": "tool_call",
                            "call_id": call_id,
                            "name": name,
                            "arguments": arguments,
                        })
                    }
                    Item::ToolResult { call_id, result } if s.tools => {
                        let mut outputs = vec![];
                        if s.full_outputs {
                            for artifact in result.artifacts {
                                match std::fs::read(&artifact.path) {
                                    Ok(bytes) => outputs.push(json!({
                                        "name": artifact.name,
                                        "media_type": artifact.media_type,
                                        "data":
                                            base64::engine::general_purpose::STANDARD.encode(bytes),
                                    })),
                                    Err(_) => warnings.push(format!(
                                        "Record {}: full output {} unavailable",
                                        record.sequence, artifact.name
                                    )),
                                }
                            }
                        }
                        json!({
                            "type": "tool_result",
                            "call_id": call_id,
                            "text": result.text,
                            "content": blocks(result.content, s),
                            "exit_code": result.exit_code,
                            "truncated": result.truncated,
                            "error": result.error.map(|e| e.message),
                            "outputs": outputs,
                        })
                    }
                    _ => continue,
                }
            }
            "provider_state" => {
                // Only known display text is eligible; signatures and continuation state stay private.
                if !s.thinking {
                    continue;
                }
                let value = &record.payload["value"];
                let raw = value.get("state").unwrap_or(value);
                let mut text = raw["thinking"]
                    .as_str()
                    .or_else(|| raw["reasoning_content"].as_str())
                    .unwrap_or_default()
                    .to_owned();
                for key in ["summary", "reasoning_details"] {
                    for part in raw[key].as_array().into_iter().flatten() {
                        if let Some(visible) =
                            part["text"].as_str().or_else(|| part["summary"].as_str())
                        {
                            text.push_str(visible);
                        }
                    }
                }
                if text.is_empty() {
                    continue;
                }
                json!({ "type": "thinking", "text": text })
            }
            "queue_delivered" if s.messages => {
                json!({
                    "type": "message",
                    "role": "user",
                    "content": blocks(
                        serde_json::from_value(record.payload["content"].clone())
                            .unwrap_or_default(),
                        s
                    ),
                })
            }
            kind if kind.contains('.') && s.extensions => {
                json!({
                    "type": "extension",
                    "summary":
                        format!("Unrecognized {kind} record; opaque payload omitted"),
                })
            }
            _ => continue,
        };
        entries.push(json!({
            "sequence": record.sequence,
            "run_id": record.run_id,
            "content": content,
        }));
    }
    for view in eden_plugin_sdk::protocol::presentation::static_views(&request.records, s) {
        entries.push(json!({
            "sequence": view
                .view
                .source
                .as_ref()
                .and_then(|source| source.record_sequence),
            "run_id": view.run_id,
            "content": { "type": "presentation", "view": view },
        }));
    }
    Ok((entries, warnings))
}
fn export(request: ExportRequest) -> Result<Artifact, Fault> {
    let (entries, warnings) = project(&request)?;
    let mut content = String::from("{\"format\":\"eden-reading-v1\",\"restorable\":false}\n");
    for entry in entries {
        content.push_str(&entry.to_string());
        content.push('\n');
    }
    Ok(Artifact {
        filename: "conversation.jsonl".into(),
        media_type: "application/x-ndjson".into(),
        content,
        warnings,
    })
}
#[cfg(test)]
mod tests;
