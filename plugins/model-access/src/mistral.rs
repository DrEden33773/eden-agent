//! Mistral keeps typed thinking and tool images in its native chat message shape.
use crate::{chat, projection, wire::failure};
use eden_protocol::{
    Fault,
    coding::{Block, Item, ModelInput, ModelReply},
    models::ModelTarget,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn project(input: &ModelInput, target: &ModelTarget) -> Result<Value, Fault> {
    let items = projection::items(input, target)?;
    // Reserve valid provider IDs before assigning replacements to foreign IDs.
    let mut used: BTreeSet<String> = items
        .iter()
        .filter_map(|item| match item {
            Item::ToolCall { call_id, .. } if valid_id(call_id) => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    let mut ids = BTreeMap::new();
    let mut names = BTreeMap::new();
    for item in &items {
        if let Item::ToolCall { call_id, name, .. } = item {
            let id = if valid_id(call_id) {
                call_id.clone()
            } else {
                fresh_id(&mut used)?
            };
            ids.entry(call_id.clone()).or_insert(id);
            names.insert(call_id.clone(), name.clone());
        }
    }
    let mut messages: Vec<Value> = Vec::new();
    for item in items {
        match item {
            Item::Message { role, content } => {
                let content = content
                    .into_iter()
                    .map(block)
                    .collect::<Result<Vec<_>, _>>()?;
                if role == "assistant" {
                    let message = assistant(&mut messages);
                    message["content"]
                        .as_array_mut()
                        .ok_or_else(|| failure("invalid Mistral content"))?
                        .extend(content);
                } else {
                    messages.push(json!({
                        "role": if role == "developer" {
                                "system"
                            } else {
                                &role
                            },
                        "content": content,
                    }));
                }
            }
            Item::ProviderState { value, .. } => {
                let raw = projection::raw_state(&value);
                if let Some(text) = raw["reasoning_content"]
                    .as_str()
                    .or_else(|| raw["thinking"].as_str())
                {
                    assistant(&mut messages)["content"]
                        .as_array_mut()
                        .ok_or_else(|| failure("invalid Mistral content"))?
                        .push(json!({
                            "type": "thinking",
                            "thinking": [{ "type": "text", "text": text }],
                        }));
                }
            }
            Item::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                let message = assistant(&mut messages);
                if message.get("tool_calls").is_none() {
                    message["tool_calls"] = json!([]);
                }
                message["tool_calls"]
                    .as_array_mut()
                    .ok_or_else(|| failure("invalid Mistral calls"))?
                    .push(json!({
                        "id": ids[&call_id],
                        "type": "function",
                        "function": { "name": name, "arguments": arguments },
                    }));
            }
            Item::ToolResult { call_id, result } => {
                // Retain structured failures/artifacts alongside native image content.
                let mut metadata = serde_json::to_value(&result)
                    .map_err(|_| failure("tool output serialization failed"))?;
                if let Some(object) = metadata.as_object_mut() {
                    object.remove("content");
                }
                let mut content = vec![json!({ "type": "text", "text": metadata.to_string() })];
                content.extend(
                    result
                        .content
                        .into_iter()
                        .map(block)
                        .collect::<Result<Vec<_>, _>>()?,
                );
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": ids[&call_id],
                    "name": names[&call_id],
                    "content": content,
                }));
            }
        }
    }
    let mut body = json!({
        "model": target.model,
        "messages": messages,
        "stream": true,
        "max_tokens": projection::output_limit(input, target)?,
    });
    if !input.tools.is_empty() {
        body["tools"] = json!(
            input
                .tools
                .iter()
                .map(|tool| json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                        "strict": false,
                    },
                }))
                .collect::<Vec<_>>()
        );
    }
    if target.capabilities.reasoning
        && let Some(level) = target
            .thinking
            .effective
            .as_deref()
            .filter(|level| *level != "off")
    {
        if matches!(
            target.model.as_str(),
            "mistral-small-2603"
                | "mistral-small-latest"
                | "mistral-medium-3.5"
                | "mistral-medium-3-5"
        ) {
            let effort = target.compat["thinkingLevelMap"][level]
                .as_str()
                .unwrap_or("high");
            if !matches!(effort, "none" | "high") {
                return Err(failure("Mistral reasoning effort must be none or high"));
            }
            body["reasoning_effort"] = json!(effort);
        } else {
            body["prompt_mode"] = json!("reasoning");
        }
    }
    Ok(body)
}

fn valid_id(id: &str) -> bool {
    id.len() == 9 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn fresh_id(used: &mut BTreeSet<String>) -> Result<String, Fault> {
    for number in 0..=used.len() {
        let candidate = format!("e{number:08x}");
        if candidate.len() != 9 {
            return Err(failure("Mistral tool identity space exhausted"));
        }
        if used.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }
    Err(failure("Mistral tool identity space exhausted"))
}

fn assistant(messages: &mut Vec<Value>) -> &mut Value {
    if messages
        .last()
        .is_none_or(|message| message["role"] != "assistant")
    {
        messages.push(json!({ "role": "assistant", "content": [], "prefix": false }));
    }
    let index = messages.len() - 1;
    &mut messages[index]
}

fn block(block: Block) -> Result<Value, Fault> {
    match block {
        Block::Text { text } => Ok(json!({ "type": "text", "text": text })),
        Block::Image { media_type, data } => Ok(json!({
            "type": "image_url",
            "image_url": format!("data:{media_type};base64,{data}"),
        })),
        Block::File { .. } => Err(failure("Mistral does not support file blocks")),
    }
}

pub(crate) struct Decoder {
    inner: chat::Decoder,
    attempt_id: String,
    closed: bool,
    ids: BTreeMap<u64, String>,
    names: BTreeMap<u64, String>,
}

impl Decoder {
    pub(crate) fn new(target: ModelTarget) -> Self {
        // Durable identities outlive a process. Random entropy plus time separates
        // reopened sessions; the counter separates concurrent attempts locally.
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let entropy = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        let attempt = crate::NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: chat::Decoder::new(target),
            attempt_id: format!("mistral{epoch:x}{entropy:016x}_{attempt:x}"),
            closed: false,
            ids: BTreeMap::new(),
            names: BTreeMap::new(),
        }
    }
    pub(crate) fn take_deltas(&mut self) -> Vec<(&'static str, Value)> {
        self.inner.take_deltas()
    }
    pub(crate) fn consume(&mut self, mut event: Value) -> Result<Option<ModelReply>, Fault> {
        if self.closed {
            return Err(failure("Mistral stream is already closed"));
        }
        let result = self
            .normalize(&mut event)
            .and_then(|()| self.inner.consume(event));
        if !matches!(result, Ok(None)) {
            self.closed = true;
        }
        result
    }
    fn normalize(&mut self, event: &mut Value) -> Result<(), Fault> {
        if event["type"] == "eden.done" || event.get("error").is_some() {
            return Ok(());
        }
        let choices = event["choices"]
            .as_array_mut()
            .ok_or_else(|| failure("invalid Mistral stream event"))?;
        for choice in choices {
            if choice["index"].as_u64().unwrap_or(0) != 0 {
                continue;
            }
            if let Some(reason) = choice["finish_reason"].as_str()
                && !matches!(reason, "stop" | "tool_calls")
            {
                return Err(failure("Mistral response did not complete"));
            }
            let delta = &mut choice["delta"];
            if let Some(chunks) = delta["content"].as_array() {
                let mut text = String::new();
                let mut thinking = String::new();
                for chunk in chunks {
                    if let Some(part) = chunk.as_str() {
                        text.push_str(part);
                        continue;
                    }
                    match chunk["type"].as_str() {
                        Some("text") => text.push_str(chunk["text"].as_str().unwrap_or("")),
                        Some("thinking") => {
                            if let Some(parts) = chunk["thinking"].as_array() {
                                for part in parts {
                                    thinking.push_str(part["text"].as_str().unwrap_or(""));
                                }
                            }
                        }
                        _ => return Err(failure("unsupported Mistral content chunk")),
                    }
                }
                delta["content"] = json!(text);
                delta["reasoning_content"] = json!(thinking);
            }
            if let Some(calls) = delta["tool_calls"].as_array_mut() {
                for call in calls {
                    let index = if let Some(index) = call["index"].as_u64() {
                        index
                    } else if let Some(id) = call["id"].as_str().filter(|id| *id != "null") {
                        self.ids
                            .iter()
                            .find_map(|(index, known)| (known == id).then_some(*index))
                            .unwrap_or(
                                self.ids
                                    .keys()
                                    .next_back()
                                    .map_or(0, |index| index.saturating_add(1)),
                            )
                    } else {
                        return Err(failure("Mistral tool delta has no identity"));
                    };
                    call["index"] = json!(index);
                    if let Some(id) = call["id"]
                        .as_str()
                        .filter(|id| !id.is_empty() && *id != "null")
                    {
                        if let Some(previous) = self.ids.get(&index) {
                            if previous != id {
                                return Err(failure("Mistral tool identity changed"));
                            }
                            call.as_object_mut()
                                .ok_or_else(|| failure("invalid Mistral tool delta"))?
                                .remove("id");
                        } else {
                            self.ids.insert(index, id.to_owned());
                        }
                    } else if !self.ids.contains_key(&index) {
                        let id = format!("{}_{index:x}", self.attempt_id);
                        self.ids.insert(index, id.clone());
                        call["id"] = json!(id);
                    } else {
                        call.as_object_mut()
                            .ok_or_else(|| failure("invalid Mistral tool delta"))?
                            .remove("id");
                    }
                    if let Some(name) = call["function"]["name"]
                        .as_str()
                        .filter(|name| !name.is_empty())
                    {
                        if let Some(previous) = self.names.get(&index) {
                            if previous != name {
                                return Err(failure("Mistral tool name changed"));
                            }
                            call["function"]
                                .as_object_mut()
                                .ok_or_else(|| failure("invalid Mistral function"))?
                                .remove("name");
                        } else {
                            self.names.insert(index, name.to_owned());
                        }
                    }
                    if call["function"]["arguments"].is_object() {
                        call["function"]["arguments"] =
                            json!(call["function"]["arguments"].to_string());
                    }
                }
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> ModelTarget {
        projection::test_target("mistral-conversations")
    }

    #[test]
    fn native_chunks_and_object_arguments_complete_only_at_terminal() {
        let mut decoder = Decoder::new(target());
        assert!(
            decoder
                .consume(json!({
                    "choices": [{
                        "delta": {
                            "content": [
                                {
                                    "type": "thinking",
                                    "thinking": [{ "type": "text", "text": "consider" }],
                                },
                                { "type": "text", "text": "answer" }
                            ],
                            "tool_calls": [{
                                "index": 0,
                                "id": "abcdef123",
                                "function": { "name": "read", "arguments": { "path": "x" } },
                            }],
                        },
                        "finish_reason": "tool_calls",
                    }],
                }))
                .unwrap()
                .is_none()
        );
        let deltas = decoder.take_deltas();
        assert!(
            deltas
                .iter()
                .any(|(kind, value)| *kind == "model_reasoning_delta"
                    && value["delta"] == "consider")
        );
        let reply = decoder
            .consume(json!({ "type": "eden.done" }))
            .unwrap()
            .unwrap();
        assert!(reply.items.iter().any(
            |item| matches!(item, Item::ToolCall{arguments,..} if arguments == "{\"path\":\"x\"}")
        ));
        let input = serde_json::from_value(json!({ "items": reply.items, "tools": [] })).unwrap();
        let body = project(&input, &target()).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["type"], "thinking");
        assert_eq!(body["messages"][0]["content"][1]["text"], "answer");
    }

    #[test]
    fn history_ids_are_paired_and_collision_safe() {
        let input: ModelInput = serde_json::from_value(json!({
            "items": [
                { "type": "tool_call", "call_id": "abcdef123", "name": "read", "arguments": "{}" },
                { "type": "tool_call", "call_id": "abc_def123", "name": "read", "arguments": "{}" },
                {
                    "type": "tool_result",
                    "call_id": "abc_def123",
                    "result": {
                        "text": "image",
                        "truncated": false,
                        "content": [{ "type": "image", "media_type": "image/png", "data": "abc" }],
                    },
                }
            ],
            "tools": [{ "name": "read", "description": "read", "parameters": { "type": "object" } }],
        })).unwrap();
        let mut target = target();
        target.capabilities.images = true;
        let body = project(&input, &target).unwrap();
        let calls = body["messages"][0]["tool_calls"].as_array().unwrap();
        assert_ne!(calls[0]["id"], calls[1]["id"]);
        for call in calls {
            let id = call["id"].as_str().unwrap();
            assert!(id.len() == 9 && id.bytes().all(|b| b.is_ascii_alphanumeric()));
        }
        assert_eq!(calls[1]["id"], body["messages"][1]["tool_call_id"]);
        assert_eq!(
            body["messages"][1]["content"][1]["image_url"],
            "data:image/png;base64,abc"
        );
        assert_eq!(body["tools"][0]["function"]["strict"], false);
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn model_specific_reasoning_and_incomplete_streams() {
        let input = serde_json::from_value(json!({ "items": [], "tools": [] })).unwrap();
        for model in [
            "mistral-small-2603",
            "mistral-small-latest",
            "mistral-medium-3.5",
            "mistral-medium-3-5",
        ] {
            let mut target = target();
            target.model = model.into();
            target.thinking.effective = Some("medium".into());
            assert_eq!(
                project(&input, &target).unwrap()["reasoning_effort"],
                "high"
            );
        }
        let mut target = target();
        target.model = "magistral-medium-latest".into();
        target.thinking.effective = Some("high".into());
        assert_eq!(
            project(&input, &target).unwrap()["prompt_mode"],
            "reasoning"
        );
        for reason in ["length", "model_length", "error", "unknown"] {
            let mut decoder = Decoder::new(target.clone());
            assert!(
                decoder
                    .consume(json!({
                        "choices": [{ "delta": { "content": "partial" }, "finish_reason": reason }],
                    }))
                    .is_err()
            );
            assert!(decoder.consume(json!({ "type": "eden.done" })).is_err());
        }
        assert!(
            Decoder::new(target)
                .consume(json!({ "type": "eden.done" }))
                .is_err()
        );
    }

    #[test]
    fn fragmented_arguments_repeated_metadata_and_index_only_identity() {
        let mut decoder = Decoder::new(target());
        decoder
            .consume(json!({
                "choices": [{
                    "delta": {
                        "content": ["text"],
                        "tool_calls": [{
                            "index": 4,
                            "function": { "name": "read", "arguments": "{\"x\":" },
                        }],
                    },
                }],
            }))
            .unwrap();
        decoder
            .consume(json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 4,
                            "function": { "name": "read", "arguments": "1}" },
                        }],
                    },
                    "finish_reason": "tool_calls",
                }],
            }))
            .unwrap();
        let reply = decoder
            .consume(json!({ "type": "eden.done" }))
            .unwrap()
            .unwrap();
        assert!(reply.items.iter().any(|item| matches!(item,Item::ToolCall{name,arguments,..} if name == "read" && arguments == "{\"x\":1}")));
        assert!(decoder.consume(json!({ "type": "eden.done" })).is_err());
    }

    #[test]
    fn provider_error_and_invalid_arguments_never_release_calls() {
        let mut decoder = Decoder::new(target());
        decoder
            .consume(json!({
                "choices": [{ "delta": { "content": "partial" }, "finish_reason": "stop" }],
            }))
            .unwrap();
        assert!(
            decoder
                .consume(json!({ "error": { "message": "provider failed" } }))
                .is_err()
        );
        assert!(decoder.consume(json!({ "type": "eden.done" })).is_err());
        let mut decoder = Decoder::new(target());
        decoder
            .consume(json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "abcdef123",
                            "function": { "name": "read", "arguments": "{" },
                        }],
                    },
                    "finish_reason": "tool_calls",
                }],
            }))
            .unwrap();
        assert!(decoder.consume(json!({ "type": "eden.done" })).is_err());
    }

    #[test]
    fn separate_responses_keep_distinct_durable_ids_and_paired_wire_ids() {
        let mut items = Vec::new();
        let mut durable_ids = Vec::new();
        for _ in 0..2 {
            let mut decoder = Decoder::new(target());
            decoder
                .consume(json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "function": { "name": "read", "arguments": "{}" },
                            }],
                        },
                        "finish_reason": "tool_calls",
                    }],
                }))
                .unwrap();
            let reply = decoder
                .consume(json!({ "type": "eden.done" }))
                .unwrap()
                .unwrap();
            let Item::ToolCall { call_id, .. } = &reply.items[0] else {
                panic!("expected tool call")
            };
            durable_ids.push(call_id.clone());
            let result = serde_json::from_value(json!({
                "type": "tool_result",
                "call_id": call_id,
                "result": { "text": "done", "truncated": false },
            }))
            .unwrap();
            items.extend(reply.items);
            items.push(result);
        }
        assert_ne!(durable_ids[0], durable_ids[1]);
        let input = serde_json::from_value(json!({ "items": items, "tools": [] })).unwrap();
        let body = project(&input, &target()).unwrap();
        let first = body["messages"][0]["tool_calls"][0]["id"].as_str().unwrap();
        let second = body["messages"][2]["tool_calls"][0]["id"].as_str().unwrap();
        assert!(valid_id(first) && valid_id(second));
        assert_ne!(first, second);
        assert_eq!(body["messages"][1]["tool_call_id"], first);
        assert_eq!(body["messages"][3]["tool_call_id"], second);
    }
}
