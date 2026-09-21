//! Radius terminal events alone release complete tool intents to the loop.
use crate::{projection, usage, wire};
use eden_protocol::{
    Fault,
    coding::{Block, Item, ModelInput, ModelReply},
    models::ModelTarget,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn blocks(content: &[Block]) -> Result<Vec<Value>, Fault> {
    content
        .iter()
        .map(|b| match b {
            Block::Text { text } => Ok(json!({ "type": "text", "text": text })),
            Block::Image { media_type, data } => Ok(json!({
                "type": "image",
                "mimeType": media_type,
                "data": data,
            })),
            Block::File { .. } => Err(wire::failure(
                "pi-messages does not support file attachments",
            )),
        })
        .collect()
}
fn assistant(content: Value, target: &ModelTarget) -> Value {
    json!({
        "role": "assistant",
        "content": [content],
        "api": target.api,
        "provider": target.provider,
        "model": target.model,
        "stopReason": "stop",
        "usage": {
            "input": 0,
            "output": 0,
            "cacheRead": 0,
            "cacheWrite": 0,
            "totalTokens": 0,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 },
        },
        "timestamp": 0,
    })
}
pub(crate) fn project(input: &ModelInput, target: &ModelTarget) -> Result<Value, Fault> {
    let mut messages = Vec::new();
    let mut system = Vec::new();
    let mut names = BTreeMap::new();
    for item in projection::items(input, target)? {
        match item {
            Item::Message { role, content } if role == "system" || role == "developer" => {
                for block in content {
                    if let Block::Text { text } = block {
                        system.push(text);
                    } else {
                        return Err(wire::failure("system attachments are unsupported"));
                    }
                }
            }
            Item::Message { role, content } if role == "assistant" => {
                let mut message = assistant(Value::Null, target);
                message["content"] = json!(blocks(&content)?);
                messages.push(message);
            }
            Item::Message { role, content } if role == "user" => messages.push(json!({
                "role": "user",
                "content": blocks(&content)?,
                "timestamp": 0,
            })),
            Item::Message { .. } => return Err(wire::failure("unsupported pi-messages role")),
            Item::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                projection::tool(&call_id, &name, &arguments)?;
                names.insert(call_id.clone(), name.clone());
                let mut message = assistant(
                    json!({
                        "type": "toolCall",
                        "id": call_id,
                        "name": name,
                        "arguments": serde_json::from_str::<Value>(&arguments)
                            .map_err(|_| wire::failure("invalid tool arguments"))?,
                    }),
                    target,
                );
                message["stopReason"] = json!("toolUse");
                messages.push(message);
            }
            Item::ToolResult { call_id, result } => {
                let mut content = blocks(&result.content)?;
                if !result.text.is_empty() {
                    content.insert(0, json!({ "type": "text", "text": result.text }));
                }
                messages.push(json!({
                    "role": "toolResult",
                    "toolCallId": call_id,
                    "toolName": names
                        .get(&call_id)
                        .ok_or_else(|| wire::failure("missing tool identity"))?,
                    "content": content,
                    "isError": result.error.is_some(),
                    "timestamp": 0,
                }));
            }
            Item::ProviderState { value, .. } => {
                let state = projection::raw_state(&value);
                if matches!(
                    state["type"].as_str(),
                    Some("pi_text_signature" | "pi_tool_metadata")
                ) {
                    let content_type = if state["type"] == "pi_text_signature" {
                        "text"
                    } else {
                        "toolCall"
                    };
                    let message = messages
                        .last_mut()
                        .filter(|message| message["role"] == "assistant")
                        .ok_or_else(|| {
                            wire::failure("content metadata has no preceding assistant message")
                        })?;
                    let block = message["content"]
                        .as_array_mut()
                        .and_then(|content| content.last_mut())
                        .filter(|block| block["type"] == content_type)
                        .ok_or_else(|| {
                            wire::failure("content metadata does not match preceding block")
                        })?;
                    let fields: &[&str] = if content_type == "text" {
                        &["textSignature"]
                    } else {
                        &["thoughtSignature", "namespace"]
                    };
                    for field in fields {
                        if let Some(value) = state.get(*field) {
                            block[*field] = value.clone();
                        }
                    }
                } else {
                    messages.push(assistant(state.clone(), target));
                }
            }
        }
    }
    Ok(json!({
        "model": target.model,
        "context": {
            "systemPrompt": system.join("\n\n"),
            "messages": messages,
            "tools": input.tools,
        },
        "options": {
            "maxTokens": projection::output_limit(input, target)?,
            "reasoning": projection::effort(target),
        },
    }))
}

pub(crate) struct Decoder {
    target: ModelTarget,
    blocks: BTreeMap<u64, Value>,
    pending: std::collections::BTreeSet<u64>,
}
impl Decoder {
    pub(crate) fn new(target: ModelTarget) -> Self {
        Self {
            target,
            blocks: BTreeMap::new(),
            pending: Default::default(),
        }
    }
    pub(crate) fn consume(&mut self, event: Value) -> Result<Option<ModelReply>, Fault> {
        let kind = event["type"].as_str().unwrap_or("");
        if kind == "error" {
            return Err(wire::provider_fault(None, &event));
        }
        if kind == "done" {
            if !matches!(event["reason"].as_str(), Some("stop" | "toolUse"))
                || !self.pending.is_empty()
            {
                return Err(wire::failure("pi-messages response is incomplete"));
            }
            let mut items = Vec::new();
            for block in self.blocks.values() {
                match block["type"].as_str() {
                    Some("text") => {
                        items.push(Item::Message {
                            role: "assistant".into(),
                            content: vec![Block::Text {
                                text: block["text"].as_str().unwrap_or("").into(),
                            }],
                        });
                        // Keep visible text in the ordinary history item. The adjacent
                        // state adds metadata on same-target replay and disappears on a switch.
                        if let Some(signature) = block["textSignature"].as_str() {
                            items.push(projection::state(
                                &self.target,
                                json!({ "type": "pi_text_signature", "textSignature": signature }),
                            ));
                        }
                    }
                    Some("thinking") => items.push(projection::state(&self.target, block.clone())),
                    Some("toolCall") => {
                        items.push(projection::tool(
                            block["id"].as_str().unwrap_or(""),
                            block["name"].as_str().unwrap_or(""),
                            &block["arguments"].to_string(),
                        )?);
                        let mut metadata = json!({ "type": "pi_tool_metadata" });
                        for field in ["thoughtSignature", "namespace"] {
                            if let Some(value) = block[field].as_str() {
                                metadata[field] = json!(value);
                            }
                        }
                        if metadata.as_object().is_some_and(|fields| fields.len() > 1) {
                            items.push(projection::state(&self.target, metadata));
                        }
                    }
                    _ => return Err(wire::failure("invalid pi-messages content")),
                }
            }
            let raw = &event["usage"];
            let mapped = json!({
                "input_tokens": raw["input"],
                "output_tokens": raw["output"],
                "cache_read_input_tokens": raw["cacheRead"],
                "cache_creation_input_tokens": raw["cacheWrite"],
                "total_tokens": raw["totalTokens"],
            });
            let mut accounting_target = self.target.clone();
            accounting_target.api = "anthropic-messages".into();
            let mut accounting =
                usage::normalize(&mapped, &accounting_target, event["reason"].as_str());
            accounting["raw"] = raw.clone();
            return Ok(Some(ModelReply {
                items,
                usage: accounting,
            }));
        }
        if kind == "start" {
            return Ok(None);
        }
        let index = event["contentIndex"]
            .as_u64()
            .ok_or_else(|| wire::failure("pi-messages event has no content index"))?;
        match kind {
            "text_start" | "thinking_start" | "toolcall_start" => {
                if self.blocks.contains_key(&index) {
                    return Err(wire::failure("duplicate pi-messages content index"));
                }
                self.pending.insert(index);
                self.blocks.insert(
                    index,
                    match kind {
                        "text_start" => json!({ "type": "text", "text": "" }),
                        "thinking_start" => json!({ "type": "thinking", "thinking": "" }),
                        _ => json!({
                            "type": "toolCall",
                            "id": event["id"],
                            "name": event["toolName"],
                        }),
                    },
                );
            }
            "text_delta" | "thinking_delta" | "toolcall_delta" => {
                if !self.pending.contains(&index) {
                    return Err(wire::failure("pi-messages delta follows content end"));
                }
                let block = self
                    .blocks
                    .get_mut(&index)
                    .ok_or_else(|| wire::failure("pi-messages delta precedes start"))?;
                let field = match kind {
                    "text_delta" => "text",
                    "thinking_delta" => "thinking",
                    _ => "partialArguments",
                };
                block[field] = json!(format!(
                    "{}{}",
                    block[field].as_str().unwrap_or(""),
                    event["delta"]
                        .as_str()
                        .ok_or_else(|| wire::failure("invalid pi-messages delta"))?
                ));
            }
            "text_end" | "thinking_end" | "toolcall_end" => {
                if !self.pending.contains(&index) {
                    return Err(wire::failure("duplicate pi-messages content end"));
                }
                let block = self
                    .blocks
                    .get_mut(&index)
                    .ok_or_else(|| wire::failure("pi-messages end precedes start"))?;
                if kind == "toolcall_end" {
                    let tool = &event["toolCall"];
                    if block["id"] != tool["id"] || block["name"] != tool["name"] {
                        return Err(wire::failure("pi-messages tool identity changed"));
                    }
                    projection::tool(
                        tool["id"].as_str().unwrap_or(""),
                        tool["name"].as_str().unwrap_or(""),
                        &tool["arguments"].to_string(),
                    )?;
                    *block = tool.clone();
                    block["type"] = json!("toolCall");
                } else {
                    let field = if kind == "text_end" {
                        "text"
                    } else {
                        "thinking"
                    };
                    block[field] = json!(
                        event["content"]
                            .as_str()
                            .ok_or_else(|| wire::failure("invalid pi-messages content"))?
                    );
                    if kind == "thinking_end" && event["redacted"].is_boolean() {
                        block["redacted"] = event["redacted"].clone();
                    }
                    if !event["contentSignature"].is_null() {
                        block[if kind == "text_end" {
                            "textSignature"
                        } else {
                            "thinkingSignature"
                        }] = event["contentSignature"].clone();
                    }
                }
                self.pending.remove(&index);
            }
            _ => return Err(wire::failure("unknown pi-messages event")),
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_tool_is_never_released_and_length_is_a_failure() {
        let target = projection::test_target("pi-messages");
        let mut decoder = Decoder::new(target);
        assert!(
            decoder
                .consume(json!({
                    "type": "toolcall_start",
                    "contentIndex": 0,
                    "id": "c",
                    "toolName": "read",
                }))
                .unwrap()
                .is_none()
        );
        decoder
            .consume(json!({ "type": "toolcall_delta", "contentIndex": 0, "delta": "{\"path\":" }))
            .unwrap();
        assert!(
            decoder
                .consume(json!({ "type": "done", "reason": "toolUse", "usage": {} }))
                .is_err()
        );
        assert!(
            decoder
                .consume(json!({ "type": "done", "reason": "length", "usage": {} }))
                .is_err()
        );
    }
    #[test]
    fn completed_tool_and_thinking_keep_identity_until_terminal() {
        let target = projection::test_target("pi-messages");
        let mut decoder = Decoder::new(target.clone());
        decoder
            .consume(json!({ "type": "thinking_start", "contentIndex": 0 }))
            .unwrap();
        decoder
            .consume(json!({
                "type": "thinking_end",
                "contentIndex": 0,
                "content": "reason",
                "contentSignature": "signature",
            }))
            .unwrap();
        decoder
            .consume(json!({
                "type": "toolcall_start",
                "contentIndex": 1,
                "id": "c",
                "toolName": "read",
            }))
            .unwrap();
        assert!(
            decoder
                .consume(json!({
                    "type": "toolcall_end",
                    "contentIndex": 1,
                    "toolCall": { "id": "c", "name": "read", "arguments": { "path": "a" } },
                }))
                .unwrap()
                .is_none()
        );
        let reply = decoder
            .consume(json!({
                "type": "done",
                "reason": "toolUse",
                "usage": {
                    "input": 2,
                    "output": 3,
                    "cacheRead": 4,
                    "cacheWrite": 0,
                    "totalTokens": 9,
                },
            }))
            .unwrap()
            .unwrap();
        assert!(matches!(&reply.items[1], Item::ToolCall{call_id,..} if call_id=="c"));
        assert!(
            serde_json::to_string(&reply.items[0])
                .unwrap()
                .contains("signature")
        );
        assert_eq!(reply.usage["raw"]["totalTokens"], 9);
    }
}

#[cfg(test)]
mod signature_tests {
    use super::*;
    #[test]
    fn signed_text_replays_once_with_metadata_only_for_same_target() {
        let target = projection::test_target("pi-messages");
        let mut decoder = Decoder::new(target.clone());
        decoder
            .consume(json!({ "type": "text_start", "contentIndex": 0 }))
            .unwrap();
        decoder
            .consume(json!({
                "type": "text_end",
                "contentIndex": 0,
                "content": "visible",
                "contentSignature": "opaque-text-signature",
            }))
            .unwrap();
        let reply = decoder
            .consume(json!({ "type": "done", "reason": "stop", "usage": {} }))
            .unwrap()
            .unwrap();
        assert_eq!(
            reply
                .items
                .iter()
                .filter(|item| matches!(item, Item::Message { .. }))
                .count(),
            1
        );
        let input = ModelInput {
            target: None,
            max_output_tokens: None,
            items: reply.items,
            tools: vec![],
        };
        let same = project(&input, &target).unwrap();
        assert_eq!(same["context"]["messages"].as_array().unwrap().len(), 1);
        assert_eq!(
            same["context"]["messages"][0]["content"],
            json!([{ "type": "text", "text": "visible", "textSignature": "opaque-text-signature" }])
        );
        let mut other = target.clone();
        other.model = "other-model".into();
        let crossed = project(&input, &other).unwrap();
        assert_eq!(crossed["context"]["messages"].as_array().unwrap().len(), 1);
        assert_eq!(
            crossed["context"]["messages"][0]["content"],
            json!([{ "type": "text", "text": "visible" }])
        );
        assert!(
            serde_json::to_string(&input)
                .unwrap()
                .contains("opaque-text-signature")
        );
    }
    #[test]
    fn signed_tool_roundtrips_reference_fields_without_duplicate_calls() {
        let target = projection::test_target("pi-messages");
        let mut decoder = Decoder::new(target.clone());
        decoder
            .consume(json!({
                "type": "toolcall_start",
                "contentIndex": 0,
                "id": "call",
                "toolName": "read",
            }))
            .unwrap();
        decoder
            .consume(json!({
                "type": "toolcall_end",
                "contentIndex": 0,
                "toolCall": {
                    "type": "toolCall",
                    "id": "call",
                    "name": "read",
                    "arguments": { "path": "a" },
                    "thoughtSignature": "opaque-tool-signature",
                    "namespace": "tools",
                },
            }))
            .unwrap();
        let reply = decoder
            .consume(json!({ "type": "done", "reason": "toolUse", "usage": {} }))
            .unwrap()
            .unwrap();
        let input = ModelInput {
            target: None,
            max_output_tokens: None,
            items: reply.items,
            tools: vec![],
        };
        assert_eq!(
            input
                .items
                .iter()
                .filter(|item| matches!(item, Item::ToolCall { .. }))
                .count(),
            1
        );
        let same = project(&input, &target).unwrap();
        assert_eq!(same["context"]["messages"].as_array().unwrap().len(), 1);
        assert_eq!(
            same["context"]["messages"][0]["content"],
            json!([{
                "type": "toolCall",
                "id": "call",
                "name": "read",
                "arguments": { "path": "a" },
                "thoughtSignature": "opaque-tool-signature",
                "namespace": "tools",
            }])
        );
        let mut other = target.clone();
        other.model = "other".into();
        let crossed = project(&input, &other).unwrap();
        assert_eq!(crossed["context"]["messages"].as_array().unwrap().len(), 1);
        assert_eq!(
            crossed["context"]["messages"][0]["content"],
            json!([{
                "type": "toolCall",
                "id": "call",
                "name": "read",
                "arguments": { "path": "a" },
            }])
        );
    }
}
