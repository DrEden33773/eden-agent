//! History projection is detached from durable records so target changes are reversible.
use crate::wire::failure;
use eden_protocol::{
    Fault,
    coding::{Block, Item, ModelInput},
    models::ModelTarget,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(crate) fn effort(target: &ModelTarget) -> Option<String> {
    target.thinking.effective.as_ref().map(|level| {
        target.compat["thinkingLevelMap"][level]
            .as_str()
            .unwrap_or(level)
            .to_owned()
    })
}
pub(crate) fn raw_state(value: &Value) -> &Value {
    value.get("state").unwrap_or(value)
}
pub(crate) fn state(target: &ModelTarget, raw: Value) -> Item {
    Item::ProviderState {
        provider: target.api.clone(),
        value: json!({
            "target": { "provider": target.provider, "model": target.model, "api": target.api },
            "state": raw,
            "thinking": target.thinking.effective,
        }),
    }
}
fn visible(value: &Value) -> String {
    let raw = raw_state(value);
    let mut text = raw["thinking"]
        .as_str()
        .or_else(|| raw["reasoning_content"].as_str())
        .unwrap_or("")
        .to_owned();
    if let Some(summary) = raw["summary"].as_array() {
        for part in summary {
            if let Some(s) = part["text"].as_str() {
                text.push_str(s);
            }
        }
    }
    if let Some(details) = raw["reasoning_details"].as_array() {
        for detail in details {
            if let Some(part) = detail["text"]
                .as_str()
                .or_else(|| detail["summary"].as_str())
            {
                text.push_str(part);
            }
        }
    }
    text
}
pub(crate) fn items(input: &ModelInput, target: &ModelTarget) -> Result<Vec<Item>, Fault> {
    let mut ids = BTreeMap::new();
    for item in &input.items {
        if let Item::ToolCall { call_id, .. } = item {
            let mut suffix = ids.len();
            let next = loop {
                let candidate = format!("eden_call_{suffix}");
                if !input.items.iter().any(
                    |item| matches!(item, Item::ToolCall { call_id, .. } if call_id == &candidate),
                ) && !ids.values().any(|id| id == &candidate)
                {
                    break candidate;
                }
                suffix += 1;
            };
            let valid = !call_id.is_empty()
                && call_id.len() <= 64
                && call_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
            ids.entry(call_id.clone())
                .or_insert(if valid { call_id.clone() } else { next });
        }
    }
    let blocks = |content: &[Block]| {
        content
            .iter()
            .map(|b| match b {
                Block::Image { .. } if !target.capabilities.images => Block::Text {
                    text: "(image omitted: model does not support images; original attachment \
                           retained in history)"
                        .into(),
                },
                other => other.clone(),
            })
            .collect()
    };
    let mut output = Vec::new();
    for item in &input.items {
        output.push(match item {
            Item::Message { role, content } => Item::Message {
                role: role.clone(),
                content: blocks(content),
            },
            Item::ToolCall {
                call_id,
                name,
                arguments,
            } => Item::ToolCall {
                call_id: ids[call_id].clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            },
            Item::ToolResult { call_id, result } => {
                let mut result = result.clone();
                result.content = blocks(&result.content);
                Item::ToolResult {
                    call_id: ids
                        .get(call_id)
                        .cloned()
                        .ok_or_else(|| failure("tool result has no matching call"))?,
                    result,
                }
            }
            Item::ProviderState { value, .. } => {
                let identity = &value["target"];
                if identity["provider"] == target.provider
                    && identity["model"] == target.model
                    && identity["api"] == target.api
                {
                    item.clone()
                } else {
                    let text = visible(value);
                    if text.is_empty() {
                        if identity.is_null() {
                            return Err(failure(
                                "legacy opaque provider state has no model identity; cannot \
                                 safely project it",
                            ));
                        }
                        continue;
                    }
                    Item::Message {
                        role: "assistant".into(),
                        content: vec![Block::Text { text }],
                    }
                }
            }
        });
    }
    Ok(output)
}
pub(crate) fn output_limit(input: &ModelInput, target: &ModelTarget) -> Result<u32, Fault> {
    let limit = input
        .max_output_tokens
        .unwrap_or(target.limits.max_output_tokens)
        .min(target.limits.max_output_tokens);
    if limit == 0 {
        Err(failure("model output allowance must be positive"))
    } else {
        Ok(limit)
    }
}
pub(crate) fn tool(call_id: &str, name: &str, arguments: &str) -> Result<Item, Fault> {
    if call_id.is_empty() || name.is_empty() {
        return Err(failure("tool call identity is empty"));
    }
    let parsed: Value = serde_json::from_str(arguments)
        .map_err(|_| failure("tool arguments are not complete JSON"))?;
    if !parsed.is_object() {
        return Err(failure("tool arguments must be an object"));
    }
    Ok(Item::ToolCall {
        call_id: call_id.into(),
        name: name.into(),
        arguments: arguments.into(),
    })
}
#[cfg(test)]
pub(crate) fn test_target(api: &str) -> ModelTarget {
    serde_json::from_value(json!({
        "provider": "test",
        "model": "test",
        "api": api,
        "base_url": "http://localhost",
        "headers": {},
        "limits": { "context_window": 8192, "max_output_tokens": 2048 },
        "thinking": { "requested": null, "effective": null },
        "capabilities": { "images": false, "tools": true, "reasoning": true },
        "pricing": null,
        "source": { "kind": "builtin", "location": "test", "updated_at": null },
    }))
    .unwrap()
}
#[cfg(test)]
mod tests {
    use super::*;
    fn input(items: Vec<Item>) -> ModelInput {
        serde_json::from_value(json!({ "items": items, "tools": [] })).unwrap()
    }
    #[test]
    fn cross_target_preserves_visible_thinking_without_signature_and_original_images() {
        let source = test_target("anthropic-messages");
        let destination = test_target("openai-completions");
        let original = input(vec![
            state(
                &source,
                json!({ "type": "thinking", "thinking": "visible", "signature": "opaque" }),
            ),
            Item::Message {
                role: "user".into(),
                content: vec![Block::Image {
                    media_type: "image/png".into(),
                    data: "abc".into(),
                }],
            },
        ]);
        let transformed = items(&original, &destination).unwrap();
        assert!(
            matches!(&transformed[0], Item::Message { content, .. } if content == &vec![Block::Text { text:"visible".into() }])
        );
        assert!(
            serde_json::to_string(&transformed[1])
                .unwrap()
                .contains("image omitted")
        );
        assert!(
            matches!(&original.items[1], Item::Message { content, .. } if matches!(content[0], Block::Image { .. }))
        );
        assert_eq!(items(&original, &source).unwrap()[0], original.items[0]);
    }
    #[test]
    fn invalid_tool_ids_map_without_colliding_with_existing_ids() {
        let target = test_target("anthropic-messages");
        let source = input(vec![
            Item::ToolCall {
                call_id: "bad|id".into(),
                name: "f".into(),
                arguments: "{}".into(),
            },
            Item::ToolCall {
                call_id: "eden_call_0".into(),
                name: "f".into(),
                arguments: "{}".into(),
            },
        ]);
        let projected = items(&source, &target).unwrap();
        assert!(
            matches!(&projected[0], Item::ToolCall { call_id, .. } if call_id == "eden_call_1")
        );
    }
}
