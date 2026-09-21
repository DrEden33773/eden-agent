//! The AWS SDK owns signing and event framing; completed messages gate tool execution.
#[cfg(test)]
mod tests;
use crate::{cloud, projection, usage, wire::failure};
use aws_sdk_bedrockruntime::{
    config::{BehaviorVersion, Region, Token, retry::RetryConfig},
    primitives::Blob,
    types::*,
};
use aws_smithy_types::{Document, Number, error::metadata::ProvideErrorMetadata};
use base64::{Engine, engine::general_purpose::STANDARD};
use eden_protocol::{
    Fault,
    coding::{Block, Item, ModelInput, ModelReply},
    models::{CredentialReply, ModelTarget},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(crate) async fn request(
    target: &ModelTarget,
    credential: &CredentialReply,
    input: &ModelInput,
    mut emit: impl FnMut(&str, Value) -> Result<(), Fault>,
) -> Result<ModelReply, Fault> {
    let region = target.compat["region"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| failure("Bedrock target requires a region"))?;
    let mut config = aws_sdk_bedrockruntime::config::Builder::new()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(region.to_owned()))
        .retry_config(RetryConfig::standard().with_max_attempts(1));
    if let Some(token) = &credential.api_key {
        config = config.bearer_token(Token::new(token, None));
    } else {
        config = config.credentials_provider(cloud::aws_credentials(credential)?);
    }
    if !target.base_url.is_empty() {
        let url = reqwest::Url::parse(&target.base_url)
            .map_err(|_| failure("invalid Bedrock endpoint"))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(failure("invalid Bedrock endpoint"));
        }
        config = config.endpoint_url(&target.base_url);
    }
    let mut headers = target.headers.clone();
    for (name, value) in &credential.headers {
        if name != cloud::AWS_PRIVATE_HEADER {
            headers.insert(name.clone(), value.clone());
        }
    }
    for (name, value) in &headers {
        if name.to_ascii_lowercase().starts_with("x-eden-private-")
            || reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err()
            || reqwest::header::HeaderValue::from_str(value).is_err()
        {
            return Err(failure("invalid Bedrock custom header"));
        }
    }
    let client = aws_sdk_bedrockruntime::Client::from_conf(config.build());
    let (messages, system, tools) = project(input, target)?;
    let limit = i32::try_from(projection::output_limit(input, target)?)
        .map_err(|_| failure("Bedrock output allowance exceeds SDK range"))?;
    let mut request = client
        .converse_stream()
        .model_id(&target.model)
        .set_messages(Some(messages))
        .set_system((!system.is_empty()).then_some(system))
        .inference_config(InferenceConfiguration::builder().max_tokens(limit).build())
        .set_tool_config(tools);
    if let Some(fields) = thinking_fields(target, limit)? {
        request = request.additional_model_request_fields(document(&fields)?);
    }
    let mut response = request
        .customize()
        .mutate_request(move |request| {
            for (name, value) in &headers {
                request.headers_mut().insert(name.clone(), value.clone());
            }
        })
        .send()
        .await
        .map_err(|error| {
            let status = error.raw_response().map(|r| r.status().as_u16());
            let retry = error
                .raw_response()
                .and_then(|r| r.headers().get("retry-after"));
            sdk_fault(&error, status, retry)
        })?;
    let mut decoder = Decoder::default();
    while let Some(event) = response
        .stream
        .recv()
        .await
        .map_err(|error| sdk_fault(&error, None, None))?
    {
        if let Some((kind, payload)) = decoder.consume(event)? {
            emit(kind, payload)?;
        }
    }
    decoder.finish(target)
}
fn thinking_fields(target: &ModelTarget, limit: i32) -> Result<Option<Value>, Fault> {
    let Some(effort) = projection::effort(target).filter(|s| s != "off") else {
        return Ok(None);
    };
    if !target.capabilities.reasoning
        || (!target.model.contains("anthropic") && target.compat["forceAdaptiveThinking"] != true)
    {
        return Ok(None);
    }
    let native = ["opus-4-7", "opus-4-8", "opus-5", "sonnet-5", "fable-5"]
        .iter()
        .any(|s| target.model.contains(s));
    let adaptive = native
        || ["opus-4-6", "sonnet-4-6"]
            .iter()
            .any(|s| target.model.contains(s))
        || target.compat["forceAdaptiveThinking"] == true;
    if adaptive {
        let effort = match effort.as_str() {
            "minimal" | "low" => "low",
            "medium" => "medium",
            "xhigh" if native => "xhigh",
            "max"
                if target
                    .thinking
                    .effective
                    .as_ref()
                    .is_some_and(|level| target.compat["thinkingLevelMap"][level] == "max") =>
            {
                "max"
            }
            _ => "high",
        };
        return Ok(Some(json!({
            "thinking": { "type": "adaptive" },
            "output_config": { "effort": effort },
        })));
    }
    if limit <= 1024 {
        return Err(failure(
            "Bedrock thinking requires more than 1024 output tokens",
        ));
    }
    let budget = match effort.as_str() {
        "minimal" => 1024,
        "low" => 2048,
        "medium" => 8192,
        _ => 16384,
    };
    Ok(Some(
        json!({ "thinking": { "type": "enabled", "budget_tokens": budget.min(limit - 1) } }),
    ))
}
fn sdk_fault<E: ProvideErrorMetadata, R>(
    error: &aws_sdk_bedrockruntime::error::SdkError<E, R>,
    status: Option<u16>,
    retry: Option<&str>,
) -> Fault {
    use aws_sdk_bedrockruntime::error::SdkError;
    let service = error.as_service_error();
    let code = service.and_then(ProvideErrorMetadata::code).unwrap_or("");
    let message = service
        .and_then(ProvideErrorMetadata::message)
        .unwrap_or("")
        .to_ascii_lowercase();
    let terminal = matches!(
        code,
        "AccessDeniedException"
            | "UnrecognizedClientException"
            | "ServiceQuotaExceededException"
            | "ResourceNotFoundException"
    ) || matches!(status, Some(401..=403));
    let overflow = code == "ValidationException"
        && (message.contains("too many tokens")
            || message.contains("input is too long")
            || message.contains("maximum context")
            || message.contains("context length"));
    let transient = matches!(
        code,
        "ThrottlingException"
            | "ServiceUnavailableException"
            | "InternalServerException"
            | "ModelNotReadyException"
            | "ModelTimeoutException"
            | "ModelStreamErrorException"
    ) || matches!(status, Some(429 | 500..=599))
        || matches!(
            error,
            SdkError::DispatchFailure(_) | SdkError::TimeoutError(_) | SdkError::ResponseError(_)
        );
    let mut fault = Fault::new(
        if terminal {
            "ProviderFailure"
        } else if overflow {
            "ContextOverflow"
        } else if transient {
            "RetryableProviderFailure"
        } else {
            "ProviderFailure"
        },
        "model-access",
        "Bedrock request failed",
    );
    fault.retry_after_ms = retry.and_then(|value| {
        value
            .parse::<u64>()
            .ok()
            .map(|n| n.saturating_mul(1000))
            .or_else(|| {
                httpdate::parse_http_date(value).ok().map(|date| {
                    date.duration_since(std::time::SystemTime::now())
                        .unwrap_or_default()
                        .as_millis()
                        .min(u64::MAX as u128) as u64
                })
            })
    });
    fault
}
fn document(value: &Value) -> Result<Document, Fault> {
    Ok(match value {
        Value::Null => Document::Null,
        Value::Bool(v) => Document::Bool(*v),
        Value::String(v) => Document::String(v.clone()),
        Value::Number(n) => Document::Number(if let Some(v) = n.as_u64() {
            Number::PosInt(v)
        } else if let Some(v) = n.as_i64() {
            Number::NegInt(v)
        } else {
            Number::Float(n.as_f64().ok_or_else(|| failure("invalid JSON number"))?)
        }),
        Value::Array(v) => Document::Array(v.iter().map(document).collect::<Result<_, _>>()?),
        Value::Object(v) => Document::Object(
            v.iter()
                .map(|(k, v)| Ok((k.clone(), document(v)?)))
                .collect::<Result<_, Fault>>()?,
        ),
    })
}
fn block(block: Block) -> Result<ContentBlock, Fault> {
    Ok(match block {
        Block::Text { text } => ContentBlock::Text(text),
        Block::Image { media_type, data } => {
            let format = match media_type.as_str() {
                "image/png" => ImageFormat::Png,
                "image/jpeg" => ImageFormat::Jpeg,
                "image/gif" => ImageFormat::Gif,
                "image/webp" => ImageFormat::Webp,
                _ => return Err(failure("unsupported Bedrock image type")),
            };
            ContentBlock::Image(
                ImageBlock::builder()
                    .format(format)
                    .source(ImageSource::Bytes(Blob::new(
                        STANDARD
                            .decode(data)
                            .map_err(|_| failure("invalid image base64"))?,
                    )))
                    .build()
                    .map_err(|_| failure("invalid Bedrock image"))?,
            )
        }
        Block::File {
            media_type, data, ..
        } => {
            let format = match media_type.as_str() {
                "application/pdf" => DocumentFormat::Pdf,
                "text/plain" => DocumentFormat::Txt,
                "text/csv" => DocumentFormat::Csv,
                "text/html" => DocumentFormat::Html,
                "text/markdown" => DocumentFormat::Md,
                _ => return Err(failure("unsupported Bedrock document type")),
            };
            ContentBlock::Document(
                DocumentBlock::builder()
                    .format(format)
                    .name("attachment")
                    .source(DocumentSource::Bytes(Blob::new(
                        STANDARD
                            .decode(data)
                            .map_err(|_| failure("invalid document base64"))?,
                    )))
                    .build()
                    .map_err(|_| failure("invalid Bedrock document"))?,
            )
        }
    })
}
fn append(
    messages: &mut Vec<Message>,
    role: ConversationRole,
    content: Vec<ContentBlock>,
) -> Result<(), Fault> {
    if let Some(last) = messages.last_mut().filter(|m| m.role == role) {
        last.content.extend(content);
    } else {
        messages.push(
            Message::builder()
                .role(role)
                .set_content(Some(content))
                .build()
                .map_err(|_| failure("invalid Bedrock message"))?,
        );
    }
    Ok(())
}
type Projection = (
    Vec<Message>,
    Vec<SystemContentBlock>,
    Option<ToolConfiguration>,
);
fn project(input: &ModelInput, target: &ModelTarget) -> Result<Projection, Fault> {
    let mut messages = Vec::new();
    let mut system = Vec::new();
    for item in projection::items(input, target)? {
        match item {
            Item::Message { role, content } if matches!(role.as_str(), "system" | "developer") => {
                for block in content {
                    let Block::Text { text } = block else {
                        return Err(failure("Bedrock system blocks must be text"));
                    };
                    system.push(SystemContentBlock::Text(text));
                }
            }
            Item::Message { role, content } => append(
                &mut messages,
                match role.as_str() {
                    "assistant" => ConversationRole::Assistant,
                    "user" => ConversationRole::User,
                    _ => return Err(failure("unsupported Bedrock message role")),
                },
                content.into_iter().map(block).collect::<Result<_, _>>()?,
            )?,
            Item::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                let input: Value = serde_json::from_str(&arguments)
                    .map_err(|_| failure("invalid Bedrock tool arguments"))?;
                append(
                    &mut messages,
                    ConversationRole::Assistant,
                    vec![ContentBlock::ToolUse(
                        ToolUseBlock::builder()
                            .tool_use_id(call_id)
                            .name(name)
                            .input(document(&input)?)
                            .build()
                            .map_err(|_| failure("invalid Bedrock tool call"))?,
                    )],
                )?;
            }
            Item::ToolResult { call_id, result } => {
                let status = if result.error.is_some() {
                    ToolResultStatus::Error
                } else {
                    ToolResultStatus::Success
                };
                let mut metadata =
                    serde_json::to_value(&result).map_err(|_| failure("invalid tool output"))?;
                metadata
                    .as_object_mut()
                    .ok_or_else(|| failure("invalid tool output"))?
                    .remove("content");
                let mut content = vec![ToolResultContentBlock::Json(document(&metadata)?)];
                for b in result.content {
                    content.push(match block(b)? {
                        ContentBlock::Text(t) => ToolResultContentBlock::Text(t),
                        ContentBlock::Image(i) => ToolResultContentBlock::Image(i),
                        ContentBlock::Document(d) => ToolResultContentBlock::Document(d),
                        _ => return Err(failure("unsupported Bedrock tool output")),
                    });
                }
                append(
                    &mut messages,
                    ConversationRole::User,
                    vec![ContentBlock::ToolResult(
                        ToolResultBlock::builder()
                            .tool_use_id(call_id)
                            .set_content(Some(content))
                            .status(status)
                            .build()
                            .map_err(|_| failure("invalid Bedrock tool result"))?,
                    )],
                )?;
            }
            Item::ProviderState { value, .. } => {
                let raw = projection::raw_state(&value);
                let reasoning = if let Some(redacted) = raw["redacted"].as_str() {
                    ReasoningContentBlock::RedactedContent(Blob::new(
                        STANDARD
                            .decode(redacted)
                            .map_err(|_| failure("invalid Bedrock redacted reasoning"))?,
                    ))
                } else {
                    ReasoningContentBlock::ReasoningText(
                        ReasoningTextBlock::builder()
                            .text(
                                raw["text"]
                                    .as_str()
                                    .ok_or_else(|| failure("invalid Bedrock reasoning"))?,
                            )
                            .set_signature(raw["signature"].as_str().map(str::to_owned))
                            .build()
                            .map_err(|_| failure("invalid Bedrock reasoning"))?,
                    )
                };
                append(
                    &mut messages,
                    ConversationRole::Assistant,
                    vec![ContentBlock::ReasoningContent(reasoning)],
                )?;
            }
        }
    }
    let tools = if input.tools.is_empty() {
        None
    } else {
        let tools = input
            .tools
            .iter()
            .map(|t| {
                Ok(Tool::ToolSpec(
                    ToolSpecification::builder()
                        .name(&t.name)
                        .description(&t.description)
                        .input_schema(ToolInputSchema::Json(document(&t.parameters)?))
                        .build()
                        .map_err(|_| failure("invalid Bedrock tool schema"))?,
                ))
            })
            .collect::<Result<_, Fault>>()?;
        Some(
            ToolConfiguration::builder()
                .set_tools(Some(tools))
                .build()
                .map_err(|_| failure("invalid Bedrock tools"))?,
        )
    };
    Ok((messages, system, tools))
}
#[derive(Default)]
struct Part {
    text: String,
    tool: Option<(String, String, String)>,
    reasoning: String,
    signature: String,
    redacted: Vec<u8>,
    stopped: bool,
}
#[derive(Default)]
struct Decoder {
    parts: BTreeMap<i32, Part>,
    started: bool,
    stop: Option<String>,
    raw: Value,
    metadata: bool,
}
impl Decoder {
    fn consume(
        &mut self,
        event: ConverseStreamOutput,
    ) -> Result<Option<(&'static str, Value)>, Fault> {
        match event {
            ConverseStreamOutput::MessageStart(e) => {
                if self.started || e.role != ConversationRole::Assistant {
                    return Err(failure("invalid Bedrock message start"));
                }
                self.started = true;
            }
            ConverseStreamOutput::ContentBlockStart(e) => {
                if !self.started
                    || self.stop.is_some()
                    || self.parts.contains_key(&e.content_block_index)
                {
                    return Err(failure("invalid Bedrock block start"));
                }
                let Some(ContentBlockStart::ToolUse(t)) = e.start else {
                    return Err(failure("unsupported Bedrock block start"));
                };
                self.parts.insert(
                    e.content_block_index,
                    Part {
                        tool: Some((t.tool_use_id, t.name, String::new())),
                        ..Default::default()
                    },
                );
            }
            ConverseStreamOutput::ContentBlockDelta(e) => {
                if !self.started || self.stop.is_some() {
                    return Err(failure("Bedrock delta outside message"));
                }
                let part = self.parts.entry(e.content_block_index).or_default();
                if part.stopped {
                    return Err(failure("Bedrock delta after block stop"));
                }
                match e.delta.ok_or_else(|| failure("missing Bedrock delta"))? {
                    ContentBlockDelta::Text(t) => {
                        if part.tool.is_some() {
                            return Err(failure("mixed Bedrock block"));
                        }
                        part.text.push_str(&t);
                        return Ok(Some(("model_text_delta", json!({ "delta": t }))));
                    }
                    ContentBlockDelta::ToolUse(t) => {
                        let tool = part
                            .tool
                            .as_mut()
                            .ok_or_else(|| failure("Bedrock tool delta without start"))?;
                        tool.2.push_str(&t.input);
                        return Ok(Some((
                            "model_tool_delta",
                            json!({ "delta": t.input, "call_id": tool.0 }),
                        )));
                    }
                    ContentBlockDelta::ReasoningContent(r) => match r {
                        ReasoningContentBlockDelta::Text(t) => {
                            part.reasoning.push_str(&t);
                            return Ok(Some(("model_reasoning_delta", json!({ "delta": t }))));
                        }
                        ReasoningContentBlockDelta::Signature(s) => part.signature.push_str(&s),
                        ReasoningContentBlockDelta::RedactedContent(b) => {
                            part.redacted.extend_from_slice(b.as_ref())
                        }
                        _ => return Err(failure("unknown Bedrock reasoning event")),
                    },
                    _ => return Err(failure("unsupported Bedrock content event")),
                }
            }
            ConverseStreamOutput::ContentBlockStop(e) => {
                let part = self
                    .parts
                    .get_mut(&e.content_block_index)
                    .ok_or_else(|| failure("Bedrock stop without block"))?;
                if part.stopped {
                    return Err(failure("duplicate Bedrock block stop"));
                }
                part.stopped = true;
            }
            ConverseStreamOutput::MessageStop(e) => {
                if !self.started || self.stop.is_some() || self.parts.values().any(|p| !p.stopped) {
                    return Err(failure("invalid Bedrock message stop"));
                }
                let reason = e.stop_reason.as_str();
                if !matches!(reason, "end_turn" | "tool_use" | "stop_sequence") {
                    return Err(failure("Bedrock response was truncated or blocked"));
                }
                self.stop = Some(reason.into());
            }
            ConverseStreamOutput::Metadata(e) => {
                if self.stop.is_none() || self.metadata {
                    return Err(failure("invalid Bedrock metadata order"));
                }
                self.metadata = true;
                if let Some(u) = e.usage {
                    self.raw = json!({
                        "inputTokens": u.input_tokens,
                        "outputTokens": u.output_tokens,
                        "totalTokens": u.total_tokens,
                        "cacheReadInputTokens": u.cache_read_input_tokens,
                        "cacheWriteInputTokens": u.cache_write_input_tokens,
                    });
                }
            }
            _ => return Err(failure("unknown Bedrock stream event")),
        }
        Ok(None)
    }
    fn finish(self, target: &ModelTarget) -> Result<ModelReply, Fault> {
        if self.stop.is_none() || !self.metadata {
            return Err(failure("incomplete Bedrock stream"));
        }
        let mut items = Vec::new();
        for part in self.parts.into_values() {
            if !part.text.is_empty() {
                items.push(Item::Message {
                    role: "assistant".into(),
                    content: vec![Block::Text { text: part.text }],
                });
            }
            if let Some((id, name, args)) = part.tool {
                if self.stop.as_deref() != Some("tool_use") {
                    return Err(failure("Bedrock tool without tool-use stop"));
                }
                items.push(projection::tool(&id, &name, &args)?);
            }
            if !part.reasoning.is_empty() || !part.signature.is_empty() {
                items.push(projection::state(
                    target,
                    json!({
                        "text": part.reasoning,
                        "thinking": part.reasoning,
                        "signature": part.signature,
                    }),
                ));
            }
            if !part.redacted.is_empty() {
                items.push(projection::state(
                    target,
                    json!({ "redacted": STANDARD.encode(part.redacted) }),
                ));
            }
        }
        Ok(ModelReply {
            items,
            usage: usage::normalize(&self.raw, target, self.stop.as_deref()),
        })
    }
}
