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
    Ok((entries, warnings))
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn render_blocks(value: &Value) -> String {
    let mut html = String::new();
    for block in value.as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => html.push_str(&format!(
                "<pre>{}</pre>",
                escape(block["text"].as_str().unwrap_or_default())
            )),
            Some("image")
                if matches!(
                    block["media_type"].as_str(),
                    Some("image/png" | "image/jpeg" | "image/gif" | "image/webp")
                ) && base64::engine::general_purpose::STANDARD
                    .decode(block["data"].as_str().unwrap_or_default())
                    .is_ok() =>
            {
                html.push_str(&format!(
                    "<img alt=\"Selected attachment\" src=\"data:{};base64,{}\">",
                    escape(block["media_type"].as_str().unwrap_or_default()),
                    escape(block["data"].as_str().unwrap_or_default())
                ))
            }
            _ => html.push_str(&format!(
                "<details><summary>Selected attachment (base64)</summary><pre>{}</pre></details>",
                escape(&block.to_string())
            )),
        }
    }
    html
}
fn export(request: ExportRequest) -> Result<Artifact, Fault> {
    let (entries, warnings) = project(&request)?;
    let (filename, media_type, content) = match request.format {
        Format::Jsonl => {
            let mut text = String::from("{\"format\":\"eden-reading-v1\",\"restorable\":false}\n");
            for entry in &entries {
                text.push_str(&entry.to_string());
                text.push('\n');
            }
            ("conversation.jsonl", "application/x-ndjson", text)
        }
        Format::Html => {
            let mut html = String::from(include_str!("template.html"));
            for entry in &entries {
                let body = &entry["content"];
                let label = body["role"]
                    .as_str()
                    .or_else(|| body["type"].as_str())
                    .unwrap_or("Record");
                html.push_str(&format!(
                    "<article><header>{} · turn {} · record {}</header>",
                    escape(label),
                    entry["run_id"],
                    entry["sequence"]
                ));
                if body["type"] == "message" {
                    html.push_str(&render_blocks(&body["content"]));
                } else {
                    html.push_str(&format!(
                        "<pre>{}</pre>",
                        escape(
                            &serde_json::to_string_pretty(body)
                                .map_err(|e| invalid(e.to_string()))?
                        )
                    ));
                }
                html.push_str("</article>");
            }
            for warning in &warnings {
                html.push_str(&format!("<p class=\"warning\">{}</p>", escape(warning)));
            }
            html.push_str("</main></body></html>");
            ("conversation.html", "text/html", html)
        }
    };
    Ok(Artifact {
        filename: filename.into(),
        media_type: media_type.into(),
        content,
        warnings,
    })
}
#[cfg(test)]
mod tests;
