//! Chat streams publish executable calls only after the terminal frame.
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_tools_wait_for_done_and_usage_tail() {
        let mut decoder = Decoder::new(crate::projection::test_target("openai-completions"));
        assert!(
            decoder
                .consume(json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "call1",
                                "function": { "name": "read", "arguments": "{\"x\":" },
                            }],
                        },
                        "finish_reason": null,
                    }],
                }))
                .unwrap()
                .is_none()
        );
        assert!(
            decoder
                .consume(json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{ "index": 0, "function": { "arguments": "1}" } }],
                        },
                        "finish_reason": "tool_calls",
                    }],
                }))
                .unwrap()
                .is_none()
        );
        decoder
            .consume(json!({
                "choices": [],
                "usage": { "prompt_tokens": 10, "completion_tokens": 3 },
            }))
            .unwrap();
        let reply = decoder
            .consume(json!({ "type": "eden.done" }))
            .unwrap()
            .unwrap();
        assert!(
            matches!(&reply.items[0], Item::ToolCall { arguments, .. } if arguments == "{\"x\":1}")
        );
        assert_eq!(reply.usage["raw"]["completion_tokens"], 3);
    }
    #[test]
    fn truncated_tool_call_never_becomes_executable() {
        let mut decoder = Decoder::new(crate::projection::test_target("openai-completions"));
        decoder
            .consume(json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "c",
                            "function": { "name": "read", "arguments": "{" },
                        }],
                    },
                    "finish_reason": "length",
                }],
            }))
            .unwrap_err();
        assert!(decoder.finish().is_err());
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
use std::collections::BTreeMap;

pub(crate) fn project(input: &ModelInput, target: &ModelTarget) -> Result<Value, Fault> {
    let mut wire_target = target.clone();
    wire_target.thinking.effective = projection::effort(target);
    let target = &wire_target;
    let mut messages: Vec<Value> = Vec::new();
    let provider = provider(target);
    let mut pending_images = Vec::new();
    for item in projection::items(input, target)? {
        if !matches!(item, Item::ToolResult { .. }) && !pending_images.is_empty() {
            messages
                .push(json!({ "role": "user", "content": std::mem::take(&mut pending_images) }));
        }
        match item {
            Item::Message { role, content } => {
                let content = content
                    .into_iter()
                    .map(|block| match block {
                        Block::Text { text } => Ok(json!({ "type": "text", "text": text })),
                        Block::Image { media_type, data } => Ok(json!({
                            "type": "image_url",
                            "image_url": { "url": format!("data:{media_type};base64,{data}") },
                        })),
                        Block::File { .. } => {
                            Err(failure("Chat Completions does not support file blocks"))
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let role = if role == "developer"
                    && !compat_bool(
                        target,
                        "supportsDeveloperRole",
                        !nonstandard_provider(target),
                    ) {
                    "system"
                } else {
                    &role
                };
                if role == "assistant"
                    && messages
                        .last()
                        .is_some_and(|m| m["role"] == "assistant" && m["content"].is_null())
                {
                    messages
                        .last_mut()
                        .ok_or_else(|| failure("missing assistant message"))?["content"] =
                        json!(content);
                } else {
                    messages.push(json!({ "role": role, "content": content }));
                }
            }
            Item::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                if messages.last().is_none_or(|m| m["role"] != "assistant") {
                    messages.push(json!({ "role": "assistant", "content": null }));
                }
                let last = messages
                    .last_mut()
                    .ok_or_else(|| failure("missing assistant message"))?;
                if last.get("tool_calls").is_none() {
                    last["tool_calls"] = json!([]);
                }
                last["tool_calls"]
                    .as_array_mut()
                    .ok_or_else(|| failure("invalid tool calls"))?
                    .push(json!({
                        "id": call_id,
                        "type": "function",
                        "function": { "name": name, "arguments": arguments },
                    }));
            }
            Item::ToolResult { call_id, result } => {
                let mut metadata = serde_json::to_value(&result)
                    .map_err(|_| failure("tool output serialization failed"))?;
                if let Some(object) = metadata.as_object_mut() {
                    object.remove("content");
                }
                let mut text = metadata.to_string();
                let mut images = Vec::new();
                for block in result.content {
                    match block {
                        Block::Text { text: part } => {
                            text.push('\n');
                            text.push_str(&part);
                        }
                        Block::Image { media_type, data } => images.push(json!({
                            "type": "image_url",
                            "image_url": { "url": format!("data:{media_type};base64,{data}") },
                        })),
                        Block::File { .. } => {
                            return Err(failure(
                                "Chat Completions does not support file tool output",
                            ));
                        }
                    }
                }
                messages.push(json!({ "role": "tool", "tool_call_id": call_id, "content": text }));
                if !images.is_empty() {
                    images.insert(
                        0,
                        json!({ "type": "text", "text": "Images from the preceding tool result:" }),
                    );
                    pending_images.extend(images);
                }
            }
            Item::ProviderState { value, .. } => {
                let raw = projection::raw_state(&value);
                let mut message = json!({ "role": "assistant", "content": null });
                if let Some(text) = raw["reasoning_content"].as_str() {
                    message[raw["reasoning_field"]
                        .as_str()
                        .filter(|field| {
                            matches!(*field, "reasoning" | "reasoning_text" | "reasoning_content")
                        })
                        .unwrap_or("reasoning_content")] = json!(text);
                }
                if raw["reasoning_details"]
                    .as_array()
                    .is_some_and(|details| !details.is_empty())
                {
                    message["reasoning_details"] = raw["reasoning_details"].clone();
                }
                messages.push(message);
            }
        }
    }
    if !pending_images.is_empty() {
        messages.push(json!({ "role": "user", "content": pending_images }));
    }
    // DeepSeek requires reasoning_content on every replayed assistant tool turn.
    if compat_bool(
        target,
        "requiresReasoningContentOnAssistantMessages",
        provider == "deepseek",
    ) {
        for message in &mut messages {
            if message["role"] == "assistant" && message.get("reasoning_content").is_none() {
                message["reasoning_content"] = json!("");
            }
        }
    }
    let tools: Vec<_> = input
        .tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                },
            })
        })
        .collect();
    let mut body = json!({
        "model": target.model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    let nonstandard = matches!(
        provider,
        "deepseek"
            | "moonshotai"
            | "moonshotai-cn"
            | "together"
            | "nvidia"
            | "zai"
            | "zai-coding-cn"
            | "ant-ling"
            | "cloudflare-ai-gateway"
    ) || target.base_url.contains("chutes.ai");
    body[target.compat["maxTokensField"]
        .as_str()
        .unwrap_or(if nonstandard {
            "max_tokens"
        } else {
            "max_completion_tokens"
        })] = json!(projection::output_limit(input, target)?);
    if let Some(effort) = &target.thinking.effective {
        let enabled = effort != "off";
        match target.compat["thinkingFormat"].as_str().unwrap_or(provider) {
            "deepseek" | "zai" | "zai-coding-cn" => {
                body["thinking"] = json!({
                    "type": if enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
                })
            }
            "qwen" => body["enable_thinking"] = json!(enabled),
            "qwen-chat-template" => {
                body["chat_template_kwargs"] = json!({
                    "enable_thinking": enabled,
                    "preserve_thinking": true,
                })
            }
            "string-thinking" => {
                body["thinking"] = json!(if enabled { effort.as_str() } else { "none" })
            }
            "together" => body["reasoning"] = json!({ "enabled": enabled }),
            "openrouter" | "ant-ling" => {
                body["reasoning"] = json!({
                    "effort": if enabled {
                            effort
                        } else {
                            "none"
                        },
                })
            }
            _ if enabled
                && compat_bool(
                    target,
                    "supportsReasoningEffort",
                    !matches!(
                        provider,
                        "xai" | "moonshotai" | "moonshotai-cn" | "nvidia" | "cloudflare-ai-gateway"
                    ),
                ) =>
            {
                body["reasoning_effort"] = json!(effort)
            }
            _ => {}
        }
    }
    if let Some(effort) = target
        .thinking
        .effective
        .as_deref()
        .filter(|effort| *effort != "off")
    {
        if provider == "deepseek" && compat_bool(target, "supportsReasoningEffort", true) {
            body["reasoning_effort"] = json!(effort);
        }
        if matches!(provider, "zai" | "zai-coding-cn") {
            body["thinking"]["clear_thinking"] = json!(false);
        }
    }
    if !compat_bool(target, "supportsUsageInStreaming", true)
        && let Some(object) = body.as_object_mut()
    {
        object.remove("stream_options");
    }
    if compat_bool(target, "supportsStore", !nonstandard_provider(target)) {
        body["store"] = json!(false);
    }
    Ok(body)
}
#[derive(Default)]
struct Call {
    id: String,
    name: String,
    arguments: String,
}
pub(crate) struct Decoder {
    target: ModelTarget,
    text: String,
    reasoning: String,
    reasoning_field: String,
    details: Vec<Value>,
    calls: BTreeMap<u64, Call>,
    raw_usage: Value,
    stop: Option<String>,
    deltas: Vec<(&'static str, Value)>,
}
impl Decoder {
    pub(crate) fn new(target: ModelTarget) -> Self {
        Self {
            target,
            text: String::new(),
            reasoning: String::new(),
            reasoning_field: "reasoning_content".into(),
            details: Vec::new(),
            calls: BTreeMap::new(),
            raw_usage: Value::Null,
            stop: None,
            deltas: Vec::new(),
        }
    }
    pub(crate) fn take_deltas(&mut self) -> Vec<(&'static str, Value)> {
        std::mem::take(&mut self.deltas)
    }
    pub(crate) fn consume(&mut self, event: Value) -> Result<Option<ModelReply>, Fault> {
        if event.get("error").is_some() {
            return Err(provider_fault(None, &event));
        }
        if event["type"] == "eden.done" {
            return self.finish().map(Some);
        }
        if let Some(u) = event.get("usage").filter(|u| !u.is_null()) {
            self.raw_usage = u.clone();
        }
        if let Some(choices) = event["choices"].as_array() {
            for choice in choices {
                if choice["index"].as_u64().unwrap_or(0) != 0 {
                    continue;
                }
                let delta = &choice["delta"];
                if let Some(text) = delta["content"]
                    .as_str()
                    .or_else(|| delta["refusal"].as_str())
                {
                    self.text.push_str(text);
                    self.deltas
                        .push(("model_text_delta", json!({ "delta": text })));
                }
                for field in ["reasoning_content", "reasoning", "reasoning_text"] {
                    if let Some(text) = delta[field].as_str() {
                        self.reasoning_field = field.into();
                        self.reasoning.push_str(text);
                        self.deltas
                            .push(("model_reasoning_delta", json!({ "delta": text })));
                        break;
                    }
                }
                if let Some(parts) = delta["reasoning_details"].as_array() {
                    for part in parts {
                        let field = match part["type"].as_str() {
                            Some("reasoning.text") => Some("text"),
                            Some("reasoning.summary") => Some("summary"),
                            _ => None,
                        };
                        if let Some(field) = field
                            && let Some(last) = self
                                .details
                                .last_mut()
                                .filter(|last| last["type"] == part["type"])
                        {
                            let text = format!(
                                "{}{}",
                                last[field].as_str().unwrap_or(""),
                                part[field].as_str().unwrap_or("")
                            );
                            last[field] = json!(text);
                            for key in ["id", "format", "index", "signature"] {
                                if last.get(key).is_none()
                                    && let Some(value) = part.get(key)
                                {
                                    last[key] = value.clone();
                                }
                            }
                        } else {
                            self.details.push(part.clone());
                        }
                    }
                }
                if let Some(calls) = delta["tool_calls"].as_array() {
                    for part in calls {
                        let index = part["index"]
                            .as_u64()
                            .ok_or_else(|| failure("tool delta missing index"))?;
                        let call = self.calls.entry(index).or_default();
                        if let Some(s) = part["id"].as_str() {
                            call.id.push_str(s);
                        }
                        if let Some(s) = part["function"]["name"].as_str() {
                            call.name.push_str(s);
                        }
                        if let Some(s) = part["function"]["arguments"].as_str() {
                            call.arguments.push_str(s);
                            self.deltas
                                .push(("model_tool_delta", json!({ "index": index, "delta": s })));
                        }
                    }
                }
                if let Some(reason) = choice["finish_reason"].as_str() {
                    if !matches!(reason, "stop" | "tool_calls" | "function_call") {
                        return Err(failure("Chat response did not complete"));
                    }
                    self.stop = Some(reason.into());
                }
            }
        }
        Ok(None)
    }
    pub(crate) fn finish(&mut self) -> Result<ModelReply, Fault> {
        if self.stop.is_none() {
            return Err(failure("Chat stream ended before finish_reason"));
        }
        let mut items = Vec::new();
        if !self.reasoning.is_empty() || !self.details.is_empty() {
            items.push(projection::state(
                &self.target,
                json!({
                    "reasoning_content": self.reasoning,
                    "reasoning_field": self.reasoning_field,
                    "reasoning_details": self.details,
                }),
            ));
        }
        if !self.text.is_empty() {
            items.push(Item::Message {
                role: "assistant".into(),
                content: vec![Block::Text {
                    text: self.text.clone(),
                }],
            });
        }
        let mut ids = std::collections::BTreeSet::new();
        for call in self.calls.values() {
            if !ids.insert(&call.id) {
                return Err(failure("duplicate tool call identity"));
            }
            items.push(projection::tool(&call.id, &call.name, &call.arguments)?);
        }
        if items.is_empty() {
            return Err(failure("Chat response output is empty"));
        }
        Ok(ModelReply {
            items,
            usage: usage::normalize(&self.raw_usage, &self.target, self.stop.as_deref()),
        })
    }
}

fn compat_bool(target: &ModelTarget, key: &str, default: bool) -> bool {
    target.compat[key].as_bool().unwrap_or(default)
}
fn nonstandard_provider(target: &ModelTarget) -> bool {
    matches!(
        provider(target),
        "nvidia"
            | "cerebras"
            | "xai"
            | "together"
            | "deepseek"
            | "zai"
            | "zai-coding-cn"
            | "moonshotai"
            | "moonshotai-cn"
            | "opencode"
            | "cloudflare-workers-ai"
            | "cloudflare-ai-gateway"
            | "ant-ling"
    ) || target.base_url.contains("chutes.ai")
}

fn provider(target: &ModelTarget) -> &str {
    for (fragment, provider) in [
        ("deepseek.com", "deepseek"),
        ("api.z.ai", "zai"),
        ("open.bigmodel.cn", "zai"),
        ("api.together.", "together"),
        ("api.moonshot.", "moonshotai"),
        ("openrouter.ai", "openrouter"),
        ("gateway.ai.cloudflare.com", "cloudflare-ai-gateway"),
        ("api.cloudflare.com", "cloudflare-workers-ai"),
        ("integrate.api.nvidia.com", "nvidia"),
        ("api.ant-ling.com", "ant-ling"),
        ("api.x.ai", "xai"),
        ("cerebras.ai", "cerebras"),
        ("opencode.ai", "opencode"),
    ] {
        if target.base_url.contains(fragment) {
            return provider;
        }
    }
    &target.provider
}
#[cfg(test)]
mod compatibility_tests {
    use super::*;
    #[test]
    fn fixed_pi_provider_compatibility_labels_select_request_fields() {
        let input: ModelInput = serde_json::from_value(json!({
            "items": [{
                "type": "message",
                "role": "developer",
                "content": [{ "type": "text", "text": "system" }],
            }],
            "tools": [],
        }))
        .unwrap();
        for (provider, max_tokens, store) in [
            ("openai", false, true),
            ("deepseek", true, false),
            ("moonshotai", true, false),
            ("moonshotai-cn", true, false),
            ("together", true, false),
            ("nvidia", true, false),
            ("zai", true, false),
            ("zai-coding-cn", true, false),
            ("ant-ling", true, false),
            ("cloudflare-ai-gateway", true, false),
            ("cerebras", false, false),
            ("xai", false, false),
            ("opencode", false, false),
            ("cloudflare-workers-ai", false, false),
            ("openrouter", false, true),
        ] {
            let mut target = projection::test_target("openai-completions");
            target.provider = provider.into();
            let body = project(&input, &target).unwrap();
            assert_eq!(body.get("max_tokens").is_some(), max_tokens, "{provider}");
            assert_eq!(body.get("store").is_some(), store, "{provider}");
        }
        let mut target = projection::test_target("openai-completions");
        target.compat = json!({
            "maxTokensField": "max_tokens",
            "supportsStore": false,
            "supportsUsageInStreaming": false,
        });
        let body = project(&input, &target).unwrap();
        assert!(body.get("max_tokens").is_some());
        assert!(body.get("stream_options").is_none());
    }
    #[test]
    fn same_target_reasoning_rejoins_assistant_tool_turn() {
        let target = projection::test_target("openai-completions");
        let input: ModelInput = serde_json::from_value(json!({
            "items": [
                projection::state(
                    &target,
                    json!({ "reasoning_content": "thought" })
                ),
                Item::Message {
                    role: "assistant".into(),
                    content: vec![Block::Text {
                        text: "text".into()
                    }]
                },
                Item::ToolCall {
                    call_id: "c".into(),
                    name: "read".into(),
                    arguments: "{}".into()
                }
            ],
            "tools": [],
        }))
        .unwrap();
        let body = project(&input, &target).unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["reasoning_content"], "thought");
        assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "c");
    }
}
