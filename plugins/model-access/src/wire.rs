//! The Responses request and reply mapping, including what is dropped and why.
use super::{Profile, RequestOptions};
use eden_protocol::{
    Fault,
    coding::{Block, Item, ModelInput, ModelReply},
};
use serde_json::{Value, json};

pub(crate) const STATE: &str = "openai-responses";
pub(crate) fn failure(message: impl Into<String>) -> Fault {
    Fault::new("ProviderFailure", "model-access", message)
}
pub(crate) fn project(
    input: &ModelInput,
    model: &str,
    options: &RequestOptions,
) -> Result<Value, Fault> {
    let items = input
        .items
        .iter()
        .map(|item| match item {
            Item::Message { role, content } => {
                if !["system", "developer", "user", "assistant"].contains(&role.as_str()) {
                    return Err(failure("unsupported message role"));
                }
                let content = content
                    .iter()
                    .map(|block| match block {
                        Block::Text { text } => Ok(if role == "assistant" {
                            json!({ "type": "output_text", "text": text, "annotations": [] })
                        } else {
                            json!({ "type": "input_text", "text": text })
                        }),
                        Block::Image { media_type, data } if role == "user" => Ok(json!({
                            "type": "input_image",
                            "image_url": format!("data:{media_type};base64,{data}"),
                        })),
                        Block::File { .. } if options.profile == Profile::Deepseek => Err(failure(
                            "DeepSeek Responses does not support file input; use text or images",
                        )),
                        Block::File {
                            name,
                            media_type,
                            data,
                        } if role == "user" => Ok(json!({
                            "type": "input_file",
                            "filename": name,
                            "file_data": format!("data:{media_type};base64,{data}"),
                        })),
                        _ => Err(failure("attachments require a user message")),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(json!({ "type": "message", "role": role, "content": content }))
            }
            Item::ToolCall {
                call_id,
                name,
                arguments,
            } => Ok(json!({
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
            })),
            Item::ToolResult { call_id, result } => {
                let output = if result.content.is_empty() {
                    Value::String(
                        serde_json::to_string(result)
                            .map_err(|_| failure("tool output serialization failed"))?,
                    )
                } else {
                    let mut metadata = serde_json::to_value(result)
                        .map_err(|_| failure("tool output serialization failed"))?;
                    if let Some(fields) = metadata.as_object_mut() {
                        fields.remove("content");
                    }
                    let mut blocks = vec![json!({
                        "type": "input_text",
                        "text": metadata.to_string(),
                    })];
                    for block in &result.content {
                        blocks.push(match block {
                            Block::Text { text } => json!({ "type": "input_text", "text": text }),
                            Block::Image { media_type, data } => json!({
                                "type": "input_image",
                                "image_url": format!("data:{media_type};base64,{data}"),
                            }),
                            Block::File { .. } if options.profile == Profile::Deepseek => {
                                return Err(failure(
                                    "DeepSeek Responses does not support file tool output",
                                ));
                            }
                            Block::File {
                                name,
                                media_type,
                                data,
                            } => json!({
                                "type": "input_file",
                                "filename": name,
                                "file_data": format!("data:{media_type};base64,{data}"),
                            }),
                        });
                    }
                    Value::Array(blocks)
                };
                Ok(json!({ "type": "function_call_output", "call_id": call_id, "output": output }))
            }
            Item::ProviderState { provider, value }
                if provider == options.profile.state() && value["type"] == "reasoning" =>
            {
                Ok(value.clone())
            }
            Item::ProviderState { .. } => Err(failure("incompatible provider state")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let tools: Vec<_> = input
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
                "strict": false,
            })
        })
        .collect();
    let mut body = json!({ "model": model, "input": items, "tools": tools, "stream": true });
    if options.profile == Profile::Openai {
        body["store"] = json!(false);
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    if input.max_output_tokens == Some(0) {
        return Err(failure(
            "request max_output_tokens must be a positive integer",
        ));
    }
    let output_limit = match (input.max_output_tokens, options.max_output_tokens) {
        (Some(requested), Some(configured)) => Some(requested.min(configured)),
        (requested, configured) => requested.or(configured),
    };
    if let Some(tokens) = output_limit {
        body["max_output_tokens"] = json!(tokens);
    }
    if let Some(effort) = &options.reasoning_effort {
        body["reasoning"] = json!({ "effort": effort });
    }
    Ok(body)
}
fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str, Fault> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| failure(format!("response missing string {key}")))
}
pub(crate) fn completed(response: &Value, profile: Profile) -> Result<ModelReply, Fault> {
    if response["status"] != "completed" {
        return Err(failure("response did not complete"));
    }
    let output = response["output"]
        .as_array()
        .ok_or_else(|| failure("response missing output"))?;
    let mut items = Vec::with_capacity(output.len());
    for item in output {
        if let Some(status) = item.get("status")
            && status != "completed"
        {
            return Err(failure("response contains unfinished output"));
        }
        match field(item, "type")? {
            "message" => {
                if field(item, "role")? != "assistant" {
                    return Err(failure("unexpected response message role"));
                }
                let mut content = Vec::new();
                for block in item["content"]
                    .as_array()
                    .ok_or_else(|| failure("response missing content"))?
                {
                    let text = match field(block, "type")? {
                        "output_text" => field(block, "text")?,
                        "refusal" => field(block, "refusal")?,
                        _ => return Err(failure("unsupported response content")),
                    };
                    content.push(Block::Text { text: text.into() });
                }
                items.push(Item::Message {
                    role: "assistant".into(),
                    content,
                });
            }
            "function_call" => {
                let arguments = field(item, "arguments")?;
                let parsed: Value = serde_json::from_str(arguments)
                    .map_err(|_| failure("tool arguments are not complete JSON"))?;
                if !parsed.is_object() {
                    return Err(failure("tool arguments must be a JSON object"));
                }
                let call_id = field(item, "call_id")?;
                let name = field(item, "name")?;
                if call_id.is_empty() || name.is_empty() {
                    return Err(failure("tool call identity is empty"));
                }
                if items.iter().any(
                    |existing| matches!(existing, Item::ToolCall { call_id: id, .. } if id == call_id),
                ) {
                    return Err(failure("duplicate tool call identity"));
                }
                items.push(Item::ToolCall {
                    call_id: call_id.into(),
                    name: name.into(),
                    arguments: arguments.into(),
                });
            }
            "reasoning" => items.push(Item::ProviderState {
                provider: profile.state().into(),
                value: item.clone(),
            }),
            _ => return Err(failure("unsupported response output")),
        }
    }
    if items.is_empty() {
        return Err(failure("response output is empty"));
    }
    Ok(ModelReply {
        items,
        usage: response.get("usage").cloned().unwrap_or(Value::Null),
    })
}

/// Keep bytes until line completion so a network read cannot split UTF-8 or CRLF.
#[derive(Default)]
pub(crate) struct Sse {
    line: Vec<u8>,
    data: String,
    after_cr: bool,
}
impl Sse {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, Fault> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.after_cr {
                self.after_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.after_cr = byte == b'\r';
                self.finish_line(&mut events)?;
            } else {
                self.line.push(byte);
            }
        }
        Ok(events)
    }
    fn finish_line(&mut self, events: &mut Vec<Value>) -> Result<(), Fault> {
        let line = std::str::from_utf8(&self.line)
            .map_err(|_| failure("stream contains invalid UTF-8"))?;
        if line.is_empty() {
            if !self.data.is_empty() {
                let data = self.data.trim_end_matches('\n');
                if data == "[DONE]" {
                    events.push(json!({ "type": "eden.done" }));
                } else {
                    events.push(
                        serde_json::from_str(data)
                            .map_err(|_| failure("stream contains invalid JSON"))?,
                    );
                }
                self.data.clear();
            }
        } else if let Some(data) = line.strip_prefix("data:") {
            self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
            self.data.push('\n');
        }
        self.line.clear();
        Ok(())
    }
}

/// Return only a classification; provider error fields may echo prompts or credentials.
pub(crate) fn provider_fault(status: Option<u16>, body: &Value) -> Fault {
    let error = body.get("error").unwrap_or(body);
    let codes = [
        error["code"].as_str().unwrap_or(""),
        error["type"].as_str().unwrap_or(""),
        error["status"].as_str().unwrap_or(""),
    ];
    let message = error["message"].as_str().unwrap_or("").to_ascii_lowercase();
    let terminal = codes.iter().any(|code| {
        matches!(
            *code,
            "insufficient_quota"
                | "billing_hard_limit_reached"
                | "billing_not_active"
                | "insufficient_balance"
                | "invalid_api_key"
                | "authentication_error"
                | "permission_denied"
                | "PERMISSION_DENIED"
                | "UNAUTHENTICATED"
        )
    });
    let terminal = terminal
        || message.contains("exceeded your current quota")
        || message.contains("insufficient balance")
        || message.contains("billing hard limit");
    let overflow = codes.iter().any(|code| {
        matches!(
            *code,
            "context_length_exceeded"
                | "context_window_exceeded"
                | "context_window_overflow"
                | "prompt_too_long"
        )
    });
    let overflow = overflow
        || message.starts_with("this model's maximum context length is")
        || message.starts_with("maximum context length exceeded")
        || message.starts_with("context window exceeded");
    let transient = codes.iter().any(|code| {
        matches!(
            *code,
            "rate_limit_exceeded"
                | "rate_limit_error"
                | "server_error"
                | "internal_server_error"
                | "overloaded_error"
                | "RESOURCE_EXHAUSTED"
                | "UNAVAILABLE"
                | "INTERNAL"
        )
    });
    let code = if terminal || matches!(status, Some(401..=403)) {
        "ProviderFailure"
    } else if overflow {
        "ContextOverflow"
    } else if transient
        || status.is_some_and(|status| status == 429 || (500..600).contains(&status))
    {
        "RetryableProviderFailure"
    } else {
        "ProviderFailure"
    };
    Fault::new(
        code,
        "model-access",
        status.map_or_else(
            || "Responses stream reported failure".into(),
            |status| format!("Responses HTTP status {status}"),
        ),
    )
}

pub(crate) fn incomplete(response: &Value, input: &ModelInput, options: &RequestOptions) -> Fault {
    let reason = response["incomplete_details"]["reason"].as_str();
    let used = response["usage"]["output_tokens"].as_u64();
    // Explicit allowances identify bounded requests such as summaries. These
    // fail atomically instead of recursively invoking history compaction.
    let recoverable = input.max_output_tokens.is_none()
        && matches!(reason, Some("max_output_tokens" | "length"))
        && used
            .zip(options.max_output_tokens)
            .is_some_and(|(used, desired)| used < u64::from(desired));
    Fault::new(
        if recoverable {
            "RecoverableLength"
        } else {
            "ProviderFailure"
        },
        "model-access",
        "Responses output is incomplete",
    )
}

/// Codex may send complete items only in output_item.done, followed by an empty
/// terminal output array. Retain those items without making tools executable.
#[derive(Default)]
pub(crate) struct ResponseOutput {
    done: std::collections::BTreeMap<u64, Value>,
    pending: std::collections::BTreeSet<u64>,
}
impl ResponseOutput {
    pub(crate) fn observe(&mut self, event: &Value) -> Result<(), Fault> {
        match event["type"].as_str() {
            Some("response.output_item.added") => {
                let index = event["output_index"]
                    .as_u64()
                    .ok_or_else(|| failure("response item missing output index"))?;
                self.pending.insert(index);
            }
            Some("response.output_item.done") => {
                let index = event["output_index"]
                    .as_u64()
                    .ok_or_else(|| failure("response item missing output index"))?;
                if !event["item"].is_object() {
                    return Err(failure("response item is not an object"));
                }
                if self.done.insert(index, event["item"].clone()).is_some() {
                    return Err(failure("duplicate completed response item"));
                }
                self.pending.remove(&index);
            }
            Some(kind) if kind.ends_with(".delta") => {
                if let Some(index) = event["output_index"].as_u64() {
                    self.pending.insert(index);
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub(crate) fn complete(&self, response: &Value, profile: Profile) -> Result<ModelReply, Fault> {
        let mut output = self.done.clone();
        let mut pending = self.pending.clone();
        if let Some(items) = response.get("output") {
            let items = items
                .as_array()
                .ok_or_else(|| failure("response output is not an array"))?;
            for (index, item) in items.iter().enumerate() {
                let index = index as u64;
                pending.remove(&index);
                if let Some(saved) = output.get_mut(&index) {
                    if saved["id"].is_string()
                        && item["id"].is_string()
                        && saved["id"] != item["id"]
                    {
                        return Err(failure("terminal response item identity changed"));
                    }
                    let fields = item
                        .as_object()
                        .ok_or_else(|| failure("terminal response item is not an object"))?;
                    let saved = saved
                        .as_object_mut()
                        .ok_or_else(|| failure("response item is not an object"))?;
                    saved.extend(fields.clone());
                } else {
                    output.insert(index, item.clone());
                }
            }
        }
        if !pending.is_empty()
            || output
                .keys()
                .enumerate()
                .any(|(index, key)| index as u64 != *key)
        {
            return Err(failure("response contains unfinished output items"));
        }
        let mut response = response.clone();
        response["output"] = json!(output.into_values().collect::<Vec<_>>());
        completed(&response, profile)
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;
    #[test]
    fn completed_items_require_terminal_success_and_complete_arguments() {
        let mut output = ResponseOutput::default();
        output
            .observe(&json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "call_id": "c",
                    "name": "read",
                    "arguments": "{",
                },
            }))
            .unwrap();
        assert!(
            output
                .complete(
                    &json!({ "status": "completed", "output": [] }),
                    Profile::Openai
                )
                .is_err()
        );
        assert!(
            output
                .complete(
                    &json!({ "status": "incomplete", "output": [] }),
                    Profile::Openai
                )
                .is_err()
        );
    }
    #[test]
    fn partial_delta_cannot_become_final_output_and_terminal_backfills_reasoning() {
        let mut output = ResponseOutput::default();
        output
            .observe(&json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": { "type": "message" },
            }))
            .unwrap();
        output
            .observe(&json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "delta": "EDEN_G3_OK",
            }))
            .unwrap();
        assert!(
            output
                .complete(
                    &json!({ "status": "completed", "output": [] }),
                    Profile::Openai
                )
                .is_err()
        );
        let mut output = ResponseOutput::default();
        output
            .observe(&json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": { "id": "r", "type": "reasoning", "summary": [] },
            }))
            .unwrap();
        let reply = output
            .complete(
                &json!({
                    "status": "completed",
                    "output": [{ "id": "r", "type": "reasoning", "encrypted_content": "signed" }],
                }),
                Profile::Openai,
            )
            .unwrap();
        assert!(
            matches!(&reply.items[0], Item::ProviderState { value, .. } if value["encrypted_content"] == "signed" && value["summary"].is_array())
        );
    }
}
