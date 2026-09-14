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
    let items = input.items.iter().map(|item| match item {
        Item::Message {role, content} => {
            if !["system","developer","user","assistant"].contains(&role.as_str()) {
                return Err(failure("unsupported message role"));
            }
            let content = content.iter().map(|block| match block {
                Block::Text {text} => Ok(if role == "assistant" {
                    json!({"type":"output_text","text":text,"annotations":[]})
                } else { json!({"type":"input_text","text":text}) }),
                Block::Image {media_type,data} if role == "user" => Ok(json!({"type":"input_image","image_url":format!("data:{media_type};base64,{data}")})),
                Block::File {..} if options.profile == Profile::Deepseek => Err(failure("DeepSeek Responses does not support file input; use text or images")),
                Block::File {name,media_type,data} if role == "user" => Ok(json!({"type":"input_file","filename":name,"file_data":format!("data:{media_type};base64,{data}")})),
                _ => Err(failure("attachments require a user message")),
            }).collect::<Result<Vec<_>,_>>()?;
            Ok(json!({"type":"message","role":role,"content":content}))
        }
        Item::ToolCall {call_id,name,arguments} => Ok(json!({"type":"function_call","call_id":call_id,"name":name,"arguments":arguments})),
        Item::ToolResult {call_id,result} => Ok(json!({"type":"function_call_output","call_id":call_id,"output":serde_json::to_string(result).map_err(|_| failure("tool output serialization failed"))?})),
        Item::ProviderState {provider,value} if provider == options.profile.state() && value["type"] == "reasoning" => Ok(value.clone()),
        Item::ProviderState {..} => Err(failure("incompatible provider state")),
    }).collect::<Result<Vec<_>,_>>()?;
    let tools: Vec<_> = input.tools.iter().map(|tool| json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.parameters,"strict":false})).collect();
    let mut body = json!({"model":model,"input":items,"tools":tools,"stream":true});
    if options.profile == Profile::Openai {
        body["store"] = json!(false);
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    if let Some(tokens) = options.max_output_tokens {
        body["max_output_tokens"] = json!(tokens);
    }
    if let Some(effort) = &options.reasoning_effort {
        body["reasoning"] = json!({"effort":effort});
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
                    |existing| matches!(existing,Item::ToolCall {call_id: id,..} if id == call_id),
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
                if data != "[DONE]" {
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
