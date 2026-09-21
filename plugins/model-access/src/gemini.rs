//! Gemini and Vertex share content projection and terminal-candidate semantics.
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
use std::collections::BTreeMap;

fn gemini_major(model: &str) -> Option<u32> {
    model
        .to_ascii_lowercase()
        .strip_prefix("gemini-")
        .and_then(|s| s.strip_prefix("live-").or(Some(s)))
        .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|s| s.parse().ok())
}
fn call_ids(target: &ModelTarget) -> bool {
    target.compat["functionCallIds"]
        .as_bool()
        .unwrap_or_else(|| {
            gemini_major(&target.model).is_some_and(|n| n >= 3)
                || target.model.starts_with("claude-")
                || target.model.starts_with("gpt-oss-")
        })
}
fn parts(content: &[Block]) -> Vec<Value> {
    content
        .iter()
        .map(|b| match b {
            Block::Text { text } => json!({ "text": text }),
            Block::Image { media_type, data }
            | Block::File {
                media_type, data, ..
            } => json!({ "inlineData": { "mimeType": media_type, "data": data } }),
        })
        .collect()
}
fn append(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = contents.last_mut().filter(|last| last["role"] == role) {
        if let Some(existing) = last["parts"].as_array_mut() {
            existing.extend(parts);
        }
    } else {
        contents.push(json!({ "role": role, "parts": parts }));
    }
}
pub(crate) fn project(input: &ModelInput, target: &ModelTarget) -> Result<Value, Fault> {
    let items = projection::items(input, target)?;
    let names: BTreeMap<_, _> = items
        .iter()
        .filter_map(|i| match i {
            Item::ToolCall { call_id, name, .. } => Some((call_id.clone(), name.clone())),
            _ => None,
        })
        .collect();
    let mut contents = Vec::new();
    let mut system = Vec::new();
    let mut replayed = false;
    for item in items {
        match item {
            Item::ProviderState { value, .. } => {
                let raw = projection::raw_state(&value);
                if let Some(p) = raw["parts"].as_array() {
                    append(&mut contents, "model", p.clone());
                    replayed = true;
                }
            }
            Item::Message { role, content } => {
                if role == "system" || role == "developer" {
                    system.extend(parts(&content));
                    continue;
                }
                if role == "assistant" && replayed {
                    continue;
                }
                replayed = false;
                append(
                    &mut contents,
                    if role == "assistant" { "model" } else { "user" },
                    parts(&content),
                );
            }
            Item::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                if replayed {
                    continue;
                }
                let args: Value = serde_json::from_str(&arguments)
                    .map_err(|_| failure("invalid Gemini tool arguments"))?;
                let mut call = json!({ "name": name, "args": args });
                if call_ids(target) {
                    call["id"] = json!(call_id);
                }
                append(
                    &mut contents,
                    "model",
                    vec![json!({ "functionCall": call })],
                );
            }
            Item::ToolResult { call_id, result } => {
                replayed = false;
                let name = names
                    .get(&call_id)
                    .ok_or_else(|| failure("Gemini tool result has no matching call"))?;
                let text: Vec<_> = result
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                let mut response = json!({
                    "name": name,
                    "response": {
                        "output": text.join("\n"),
                        "text": result.text,
                        "details": result.details,
                        "error": result.error,
                        "exit_code": result.exit_code,
                        "truncated": result.truncated,
                    },
                });
                if call_ids(target) {
                    response["id"] = json!(call_id);
                }
                let images: Vec<_> = parts(&result.content)
                    .into_iter()
                    .filter(|p| p.get("inlineData").is_some())
                    .collect();
                let nested = gemini_major(&target.model).is_none_or(|n| n >= 3);
                if nested && !images.is_empty() {
                    response["parts"] = json!(images);
                }
                let mut p = vec![json!({ "functionResponse": response })];
                if !nested {
                    p.extend(images);
                }
                append(&mut contents, "user", p);
            }
        }
    }
    let mut body = json!({
        "contents": contents,
        "generationConfig": { "maxOutputTokens": projection::output_limit(input, target)? },
    });
    if !system.is_empty() {
        body["systemInstruction"] = json!({ "parts": system });
    }
    if !input.tools.is_empty() {
        body["tools"] = json!([{
            "functionDeclarations": input
                .tools
                .iter()
                .map(|t| json!({
                    "name": t.name,
                    "description": t.description,
                    "parametersJsonSchema": t.parameters,
                }))
                .collect::<Vec<_>>(),
        }]);
    }
    if let Some(effort) = projection::effort(target).filter(|_| target.capabilities.reasoning) {
        let effort = effort.to_ascii_lowercase();
        let level_model = gemini_major(&target.model).is_some_and(|n| n >= 3)
            || target.model.starts_with("gemini-flash-")
            || target.model.starts_with("gemini-pro-")
            || target.model.contains("gemma-4");
        let config = if level_model {
            let level = if effort == "off" {
                if target.model.contains("pro") {
                    "LOW"
                } else {
                    "MINIMAL"
                }
            } else if target.model.contains("pro") {
                if matches!(effort.as_str(), "minimal" | "low") {
                    "LOW"
                } else {
                    "HIGH"
                }
            } else if target.model.contains("gemma-4") {
                if matches!(effort.as_str(), "minimal" | "low") {
                    "MINIMAL"
                } else {
                    "HIGH"
                }
            } else {
                match effort.as_str() {
                    "minimal" => "MINIMAL",
                    "low" => "LOW",
                    "medium" => "MEDIUM",
                    _ => "HIGH",
                }
            };
            if effort == "off" {
                json!({ "thinkingLevel": level })
            } else {
                json!({ "includeThoughts": true, "thinkingLevel": level })
            }
        } else if effort == "off" {
            json!({ "thinkingBudget": 0 })
        } else {
            let budget = if target.model.contains("2.5-") {
                match effort.as_str() {
                    "minimal" => {
                        if target.model.contains("flash-lite") {
                            512
                        } else {
                            128
                        }
                    }
                    "low" => 2048,
                    "medium" => 8192,
                    _ => {
                        if target.model.contains("pro") {
                            32768
                        } else {
                            24576
                        }
                    }
                }
            } else {
                -1
            };
            json!({ "includeThoughts": true, "thinkingBudget": budget })
        };
        body["generationConfig"]["thinkingConfig"] = config;
    }
    Ok(body)
}
pub(crate) struct Decoder {
    target: ModelTarget,
    parts: Vec<Value>,
    calls: Vec<Item>,
    text: String,
    thinking: String,
    deltas: Vec<(&'static str, Value)>,
    raw_usage: Value,
}
impl Decoder {
    pub(crate) fn new(target: ModelTarget) -> Self {
        Self {
            target,
            parts: vec![],
            calls: vec![],
            text: String::new(),
            thinking: String::new(),
            deltas: vec![],
            raw_usage: Value::Null,
        }
    }
    pub(crate) fn take_deltas(&mut self) -> Vec<(&'static str, Value)> {
        std::mem::take(&mut self.deltas)
    }
    pub(crate) fn consume(&mut self, event: Value) -> Result<Option<ModelReply>, Fault> {
        if event.get("error").is_some() {
            return Err(provider_fault(None, &event));
        }
        if event["promptFeedback"].get("blockReason").is_some() {
            return Err(failure("Gemini prompt was blocked"));
        }
        if let Some(u) = event.get("usageMetadata") {
            self.raw_usage = u.clone();
        }
        let candidate = &event["candidates"][0];
        if let Some(parts) = candidate["content"]["parts"].as_array() {
            for part in parts {
                if let Some(text) = part["text"].as_str() {
                    let thinking = part["thought"] == true;
                    if thinking {
                        self.thinking.push_str(text);
                    } else {
                        self.text.push_str(text);
                    }
                    self.deltas.push((
                        if thinking {
                            "model_reasoning_delta"
                        } else {
                            "model_text_delta"
                        },
                        json!({ "delta": text }),
                    ));
                }
                if let Some(call) = part.get("functionCall") {
                    let id = call["id"].as_str().map(str::to_owned).unwrap_or_else(|| {
                        format!(
                            "gemini_{:x}_{}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_nanos(),
                            crate::NEXT_ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        )
                    });
                    if self
                        .calls
                        .iter()
                        .any(|c| matches!(c,Item::ToolCall{call_id,..} if call_id==&id))
                    {
                        return Err(failure("duplicate Gemini tool call identity"));
                    }
                    let args = call
                        .get("args")
                        .cloned()
                        .unwrap_or_else(|| json!({}))
                        .to_string();
                    self.calls.push(projection::tool(
                        &id,
                        call["name"].as_str().unwrap_or(""),
                        &args,
                    )?);
                    self.deltas
                        .push(("model_tool_delta", json!({ "call_id": id, "delta": args })));
                }
                if part.get("text").is_none()
                    && part.get("functionCall").is_none()
                    && part.get("thoughtSignature").is_none()
                {
                    return Err(failure("unsupported Gemini output part"));
                }
                let mut retained = part.clone();
                if retained.get("functionCall").is_some()
                    && call_ids(&self.target)
                    && let Some(Item::ToolCall { call_id, .. }) = self.calls.last()
                {
                    retained["functionCall"]["id"] = json!(call_id);
                }
                self.parts.push(retained);
            }
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            if reason != "STOP" {
                return Err(failure("Gemini response did not complete"));
            }
            if self.text.is_empty() && self.calls.is_empty() {
                return Err(failure("Gemini response output is empty"));
            }
            let mut items = vec![projection::state(
                &self.target,
                json!({ "parts": self.parts, "thinking": self.thinking }),
            )];
            if !self.text.is_empty() {
                items.push(Item::Message {
                    role: "assistant".into(),
                    content: vec![Block::Text {
                        text: self.text.clone(),
                    }],
                });
            }
            items.append(&mut self.calls);
            return Ok(Some(ModelReply {
                items,
                usage: usage::normalize(&self.raw_usage, &self.target, Some(reason)),
            }));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::test_target;
    #[test]
    fn custom_pro_and_gemma_targets_map_supported_thinking_levels() {
        let input: ModelInput =
            serde_json::from_value(json!({ "items": [], "tools": [] })).unwrap();
        for (model, level, expected) in [
            ("gemini-3.1-pro", "minimal", "LOW"),
            ("gemini-3-pro", "medium", "HIGH"),
            ("gemma-4-31b-it", "low", "MINIMAL"),
            ("gemma-4-31b-it", "medium", "HIGH"),
        ] {
            let mut target = test_target("google-generative-ai");
            target.model = model.into();
            target.thinking.effective = Some(level.into());
            assert_eq!(
                project(&input, &target).unwrap()["generationConfig"]["thinkingConfig"]["thinkingLevel"],
                expected
            );
        }
    }
    #[test]
    fn gemini_three_replays_paired_ids_and_keeps_each_tools_image_attached() {
        let mut target = test_target("google-generative-ai");
        target.model = "gemini-3-flash".into();
        target.capabilities.images = true;
        let reply = Decoder::new(target.clone())
            .consume(json!({
                "candidates": [{
                    "content": {
                        "parts": [
                            {
                                "functionCall": { "name": "read", "id": "a", "args": {} },
                                "thoughtSignature": "c2ln",
                            },
                            { "functionCall": { "name": "read", "id": "b", "args": {} } }
                        ],
                    },
                    "finishReason": "STOP",
                }],
            }))
            .unwrap()
            .unwrap();
        let mut items = reply.items;
        for (call_id, data) in [("a", "YQ=="), ("b", "Yg==")] {
            items.push(Item::ToolResult {
                call_id: call_id.into(),
                result: serde_json::from_value(json!({
                    "text": "image",
                    "truncated": false,
                    "content": [{ "type": "image", "media_type": "image/png", "data": data }],
                }))
                .unwrap(),
            });
        }
        let input: ModelInput =
            serde_json::from_value(json!({ "items": items, "tools": [] })).unwrap();
        let body = project(&input, &target).unwrap();
        assert_eq!(body["contents"][0]["parts"][0]["functionCall"]["id"], "a");
        for (index, id, data) in [(0, "a", "YQ=="), (1, "b", "Yg==")] {
            let response = &body["contents"][1]["parts"][index]["functionResponse"];
            assert_eq!(response["id"], id);
            assert_eq!(response["parts"][0]["inlineData"]["data"], data);
        }
    }
    #[test]
    fn separate_responses_never_reuse_synthesized_tool_ids() {
        let response = json!({
            "candidates": [{
                "content": { "parts": [{ "functionCall": { "name": "read", "args": {} } }] },
                "finishReason": "STOP",
            }],
        });
        let run = || {
            Decoder::new(test_target("google-generative-ai"))
                .consume(response.clone())
                .unwrap()
                .unwrap()
                .items
                .into_iter()
                .find_map(|i| {
                    if let Item::ToolCall { call_id, .. } = i {
                        Some(call_id)
                    } else {
                        None
                    }
                })
                .unwrap()
        };
        assert_ne!(run(), run());
    }
    #[test]
    fn model_specific_thinking_uses_native_levels_and_budgets() {
        let input: ModelInput =
            serde_json::from_value(json!({ "items": [], "tools": [] })).unwrap();
        for (model, effort, expected) in [
            ("gemini-3-pro", "off", json!({ "thinkingLevel": "LOW" })),
            (
                "gemini-3-flash",
                "medium",
                json!({ "includeThoughts": true, "thinkingLevel": "MEDIUM" }),
            ),
            (
                "gemini-2.5-pro",
                "high",
                json!({ "includeThoughts": true, "thinkingBudget": 32768 }),
            ),
            (
                "gemini-2.5-flash-lite",
                "minimal",
                json!({ "includeThoughts": true, "thinkingBudget": 512 }),
            ),
        ] {
            let mut t = test_target("google-generative-ai");
            t.model = model.into();
            t.thinking.effective = Some(effort.into());
            assert_eq!(
                project(&input, &t).unwrap()["generationConfig"]["thinkingConfig"],
                expected
            );
        }
    }
    #[test]
    fn tools_wait_for_finish_and_signatures_survive_same_target_replay() {
        let target = test_target("google-generative-ai");
        let mut decoder = Decoder::new(target.clone());
        assert!(
            decoder
                .consume(json!({
                    "candidates": [{
                        "content": {
                            "parts": [{
                                "functionCall": { "name": "read", "args": { "path": "a" } },
                                "thoughtSignature": "signature",
                            }],
                        },
                    }],
                }))
                .unwrap()
                .is_none()
        );
        let reply = decoder
            .consume(json!({
                "candidates": [{ "finishReason": "STOP" }],
                "usageMetadata": {
                    "promptTokenCount": 10,
                    "candidatesTokenCount": 2,
                    "thoughtsTokenCount": 3,
                    "cachedContentTokenCount": 4,
                    "totalTokenCount": 15,
                },
            }))
            .unwrap()
            .unwrap();
        let call = reply
            .items
            .iter()
            .find(|i| matches!(i, Item::ToolCall { .. }))
            .unwrap();
        let Item::ToolCall { call_id, .. } = call else {
            unreachable!()
        };
        let mut items = reply.items.clone();
        items.push(
            serde_json::from_value(json!({
                "type": "tool_result",
                "call_id": call_id,
                "result": {
                    "content": [{ "type": "text", "text": "file" }],
                    "text": "file",
                    "truncated": false,
                },
            }))
            .unwrap(),
        );
        let input: ModelInput =
            serde_json::from_value(json!({ "items": items, "tools": [] })).unwrap();
        let body = project(&input, &target).unwrap();
        assert_eq!(
            body["contents"][0]["parts"][0]["thoughtSignature"],
            "signature"
        );
        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"]["name"],
            "read"
        );
        assert_eq!(reply.usage["normalized"]["output_tokens"], 5);
        assert_eq!(reply.usage["normalized"]["input_tokens"], 6);
    }
    #[test]
    fn blocked_and_truncated_candidates_never_execute_tools() {
        for reason in ["MAX_TOKENS", "SAFETY", "MALFORMED_FUNCTION_CALL"] {
            let mut decoder = Decoder::new(test_target("google-generative-ai"));
            assert!(
                decoder
                    .consume(json!({
                        "candidates": [{
                            "content": {
                                "parts": [{ "functionCall": { "name": "f", "args": {} } }],
                            },
                            "finishReason": reason,
                        }],
                    }))
                    .is_err()
            );
        }
    }
    #[test]
    fn images_and_system_instructions_use_native_parts() {
        let mut target = test_target("google-generative-ai");
        target.capabilities.images = true;
        let input: ModelInput = serde_json::from_value(json!({
            "items": [
                {
                    "type": "message",
                    "role": "system",
                    "content": [{ "type": "text", "text": "rules" }],
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "image", "media_type": "image/png", "data": "YWJj" }],
                }
            ],
            "tools": [],
        }))
        .unwrap();
        let body = project(&input, &target).unwrap();
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "rules");
        assert_eq!(
            body["contents"][0]["parts"][0]["inlineData"]["data"],
            "YWJj"
        );
    }
}
