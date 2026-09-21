//! Anthropic block state is retained until message_stop confirms completion.
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tool_json_is_atomic_at_message_stop() {
        let mut decoder = Decoder::new(crate::projection::test_target("anthropic-messages"));
        decoder
            .consume(json!({
                "type": "message_start",
                "message": { "usage": { "input_tokens": 4 } },
            }))
            .unwrap();
        decoder
            .consume(json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "tool_use", "id": "c", "name": "read", "input": {} },
            }))
            .unwrap();
        assert!(
            decoder
                .consume(json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "input_json_delta", "partial_json": "{\"a\":1}" },
                }))
                .unwrap()
                .is_none()
        );
        decoder
            .consume(json!({ "type": "content_block_stop", "index": 0 }))
            .unwrap();
        decoder
            .consume(json!({
                "type": "message_delta",
                "delta": { "stop_reason": "tool_use" },
                "usage": { "output_tokens": 7 },
            }))
            .unwrap();
        let reply = decoder
            .consume(json!({ "type": "message_stop" }))
            .unwrap()
            .unwrap();
        assert!(
            matches!(&reply.items[0], Item::ToolCall {arguments,..} if arguments == "{\"a\":1}")
        );
    }
}
use crate::{
    projection, usage,
    wire::{failure, provider_fault},
};
use eden_protocol::{
    Fault,
    coding::{Block, Item, ModelInput, ModelReply},
    models::ModelTarget,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
fn blocks(content: Vec<Block>) -> Result<Vec<Value>, Fault> {
    content
        .into_iter()
        .map(|b| match b {
            Block::Text { text } => Ok(json!({ "type": "text", "text": text })),
            Block::Image { media_type, data } => Ok(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media_type, "data": data },
            })),
            Block::File { .. } => Err(failure(
                "Anthropic file input is unsupported; provide text or images",
            )),
        })
        .collect()
}
fn append(messages: &mut Vec<Value>, role: &str, content: Vec<Value>) {
    if let Some(last) = messages.last_mut()
        && last["role"] == role
        && let Some(existing) = last["content"].as_array_mut()
    {
        existing.extend(content);
        return;
    }
    messages.push(json!({ "role": role, "content": content }));
}
pub(crate) fn project(input: &ModelInput, target: &ModelTarget) -> Result<Value, Fault> {
    let mut wire_target = target.clone();
    wire_target.thinking.effective = projection::effort(target);
    let target = &wire_target;
    let mut messages = Vec::new();
    let mut system = Vec::new();
    for item in projection::items(input, target)? {
        match item {
            Item::Message { role, content } => {
                let content = blocks(content)?;
                match role.as_str() {
                    "system" | "developer" => system.extend(content),
                    "user" | "assistant" => append(&mut messages, &role, content),
                    _ => return Err(failure("unsupported message role")),
                }
            }
            Item::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                let arguments: Value = serde_json::from_str(&arguments)
                    .map_err(|_| failure("historical tool arguments are invalid JSON"))?;
                append(
                    &mut messages,
                    "assistant",
                    vec![json!({
                        "type": "tool_use",
                        "id": call_id,
                        "name": name,
                        "input": arguments,
                    })],
                );
            }
            Item::ToolResult { call_id, result } => {
                let mut metadata = serde_json::to_value(&result)
                    .map_err(|_| failure("tool result serialization failed"))?;
                if let Some(object) = metadata.as_object_mut() {
                    object.remove("content");
                }
                let mut content = vec![json!({ "type": "text", "text": metadata.to_string() })];
                content.extend(blocks(result.content)?);
                append(
                    &mut messages,
                    "user",
                    vec![json!({
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": content,
                        "is_error": result.error.is_some(),
                    })],
                );
            }
            Item::ProviderState { value, .. } => {
                if target.compat["supportsMidConvoEffort"] == true
                    && let Some(effort) = value["thinking"].as_str()
                {
                    messages.push(json!({
                        "role": "system",
                        "content": [],
                        "output_config": { "effort": effort },
                    }));
                }
                append(
                    &mut messages,
                    "assistant",
                    vec![projection::raw_state(&value).clone()],
                );
            }
        }
    }
    if target.compat["supportsMidConvoEffort"] == true {
        let effort = target.thinking.effective.as_deref().unwrap_or("high");
        messages.push(json!({
            "role": "system",
            "content": [],
            "output_config": {
                "effort": if effort == "minimal" {
                        "low"
                    } else {
                        effort
                    },
            },
        }));
    }
    let tools: Vec<_> = input
        .tools
        .iter()
        .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.parameters }))
        .collect();
    let limit = projection::output_limit(input, target)?;
    let mut body = json!({
        "model": target.model,
        "messages": messages,
        "max_tokens": limit,
        "stream": true,
    });
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    if target.compat["supportsMidConvoEffort"] == true {
        body["thinking"] = json!({
            "type": "adaptive",
            "display": "summarized",
            "block_binding": { "prefix_mismatch_behavior": "drop_block" },
        });
        body["output_config"] = json!({ "effort": "high" });
    } else if target.capabilities.reasoning {
        if let Some(effort) = target.thinking.effective.as_deref().filter(|e| *e != "off") {
            if target.compat["forceAdaptiveThinking"] == true {
                body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
                body["output_config"] = json!({
                    "effort": if effort == "minimal" {
                            "low"
                        } else {
                            effort
                        },
                });
            } else {
                let budget = match effort {
                    "minimal" | "low" => 1024,
                    "medium" => 4096,
                    "high" => 8192,
                    _ => 16384,
                };
                if limit <= 1024 {
                    return Err(failure(
                        "Anthropic thinking requires output allowance above 1024",
                    ));
                }
                body["thinking"] = json!({
                    "type": "enabled",
                    "budget_tokens": budget.min(limit - 1),
                });
            }
        } else {
            body["thinking"] = json!({ "type": "disabled" });
        }
    }
    Ok(body)
}
struct Content {
    value: Value,
    json: String,
    closed: bool,
}
pub(crate) struct Decoder {
    target: ModelTarget,
    started: bool,
    content: BTreeMap<u64, Content>,
    raw_usage: Value,
    stop: Option<String>,
    deltas: Vec<(&'static str, Value)>,
}
impl Decoder {
    pub(crate) fn new(target: ModelTarget) -> Self {
        Self {
            target,
            started: false,
            content: BTreeMap::new(),
            raw_usage: json!({}),
            stop: None,
            deltas: Vec::new(),
        }
    }
    pub(crate) fn take_deltas(&mut self) -> Vec<(&'static str, Value)> {
        std::mem::take(&mut self.deltas)
    }
    pub(crate) fn consume(&mut self, event: Value) -> Result<Option<ModelReply>, Fault> {
        match event["type"]
            .as_str()
            .ok_or_else(|| failure("Anthropic event missing type"))?
        {
            "error" => return Err(provider_fault(None, &event)),
            "message_start" => {
                if self.started {
                    return Err(failure("duplicate message_start"));
                }
                self.started = true;
                self.raw_usage = event["message"]["usage"].clone();
            }
            "content_block_start" => {
                let index = event["index"]
                    .as_u64()
                    .ok_or_else(|| failure("block missing index"))?;
                if self
                    .content
                    .insert(
                        index,
                        Content {
                            value: event["content_block"].clone(),
                            json: String::new(),
                            closed: false,
                        },
                    )
                    .is_some()
                {
                    return Err(failure("duplicate content block"));
                }
            }
            "content_block_delta" => {
                let index = event["index"]
                    .as_u64()
                    .ok_or_else(|| failure("block delta missing index"))?;
                let block = self
                    .content
                    .get_mut(&index)
                    .ok_or_else(|| failure("delta precedes content block"))?;
                if block.closed {
                    return Err(failure("delta follows closed block"));
                }
                let delta = &event["delta"];
                match delta["type"].as_str() {
                    Some("text_delta" | "thinking_delta" | "signature_delta") => {
                        let (field, kind) = match delta["type"].as_str() {
                            Some("text_delta") => ("text", Some("model_text_delta")),
                            Some("thinking_delta") => ("thinking", Some("model_reasoning_delta")),
                            _ => ("signature", None),
                        };
                        let text = delta[field]
                            .as_str()
                            .ok_or_else(|| failure("block delta missing text"))?;
                        let mut accumulated = block.value[field].as_str().unwrap_or("").to_owned();
                        accumulated.push_str(text);
                        block.value[field] = json!(accumulated);
                        if let Some(kind) = kind {
                            self.deltas
                                .push((kind, json!({ "index": index, "delta": text })));
                        }
                    }
                    Some("input_json_delta") => {
                        let text = delta["partial_json"]
                            .as_str()
                            .ok_or_else(|| failure("tool delta missing JSON"))?;
                        block.json.push_str(text);
                        self.deltas
                            .push(("model_tool_delta", json!({ "index": index, "delta": text })));
                    }
                    _ => return Err(failure("unsupported Anthropic content delta")),
                }
            }
            "content_block_stop" => {
                let index = event["index"]
                    .as_u64()
                    .ok_or_else(|| failure("block stop missing index"))?;
                let block = self
                    .content
                    .get_mut(&index)
                    .ok_or_else(|| failure("stop precedes block"))?;
                if block.closed {
                    return Err(failure("duplicate block stop"));
                }
                block.closed = true;
            }
            "message_delta" => {
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    if !matches!(reason, "end_turn" | "tool_use" | "stop_sequence") {
                        return Err(failure("Anthropic response did not complete"));
                    }
                    self.stop = Some(reason.into());
                }
                if let Some(usage) = event["usage"].as_object() {
                    if !self.raw_usage.is_object() {
                        self.raw_usage = json!({});
                    }
                    for (k, v) in usage {
                        self.raw_usage[k] = v.clone();
                    }
                }
            }
            "message_stop" => return self.finish().map(Some),
            "ping" => {}
            _ => {}
        }
        Ok(None)
    }
    pub(crate) fn finish(&mut self) -> Result<ModelReply, Fault> {
        if !self.started || self.stop.is_none() || self.content.values().any(|b| !b.closed) {
            return Err(failure("Anthropic stream ended before complete message"));
        }
        let mut items = Vec::new();
        let mut ids = BTreeSet::new();
        for block in self.content.values() {
            let value = &block.value;
            match value["type"].as_str() {
                Some("text") => items.push(Item::Message {
                    role: "assistant".into(),
                    content: vec![Block::Text {
                        text: value["text"]
                            .as_str()
                            .ok_or_else(|| failure("text block missing text"))?
                            .into(),
                    }],
                }),
                Some("thinking" | "redacted_thinking") => {
                    items.push(projection::state(&self.target, value.clone()))
                }
                Some("tool_use") => {
                    let id = value["id"]
                        .as_str()
                        .ok_or_else(|| failure("tool block missing id"))?;
                    if !ids.insert(id) {
                        return Err(failure("duplicate tool call identity"));
                    }
                    let initial = value["input"].to_string();
                    let arguments = if block.json.is_empty() {
                        &initial
                    } else {
                        &block.json
                    };
                    items.push(projection::tool(
                        id,
                        value["name"].as_str().unwrap_or(""),
                        arguments,
                    )?);
                }
                _ => return Err(failure("unsupported Anthropic response block")),
            }
        }
        if items.is_empty() {
            return Err(failure("Anthropic response output is empty"));
        }
        Ok(ModelReply {
            items,
            usage: usage::normalize(&self.raw_usage, &self.target, self.stop.as_deref()),
        })
    }
}
#[cfg(test)]
mod compatibility_tests {
    use super::*;
    #[test]
    fn adaptive_thinking_uses_catalog_compat_and_signature_survives_same_target() {
        let mut target = projection::test_target("anthropic-messages");
        target.compat = json!({ "forceAdaptiveThinking": true });
        target.thinking.effective = Some("high".into());
        let state = projection::state(
            &target,
            json!({ "type": "thinking", "thinking": "visible", "signature": "secret-signature" }),
        );
        let input: ModelInput =
            serde_json::from_value(json!({ "items": [state], "tools": [] })).unwrap();
        let body = project(&input, &target).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(
            body["messages"][0]["content"][0]["signature"],
            "secret-signature"
        );
        target.model = "other".into();
        let body = project(&input, &target).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert!(!body.to_string().contains("secret-signature"));
    }
    #[test]
    fn open_blocks_prevent_tool_completion() {
        let mut decoder = Decoder::new(projection::test_target("anthropic-messages"));
        decoder
            .consume(json!({ "type": "message_start", "message": { "usage": {} } }))
            .unwrap();
        decoder
            .consume(json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "tool_use", "id": "c", "name": "read", "input": {} },
            }))
            .unwrap();
        decoder
            .consume(json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" } }))
            .unwrap();
        assert!(decoder.consume(json!({ "type": "message_stop" })).is_err());
    }
}
