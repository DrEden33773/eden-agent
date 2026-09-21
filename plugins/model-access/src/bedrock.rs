//! The AWS SDK owns signing and event framing; completed messages gate tool execution.
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incomplete_stream_cannot_publish_a_tool() {
        let decoder = Decoder::default();
        assert!(
            decoder
                .finish(&projection::test_target("bedrock-converse-stream"))
                .is_err()
        );
    }
    #[test]
    fn base64_images_are_decoded_and_unsupported_types_rejected() {
        let image = block(Block::Image {
            media_type: "image/png".into(),
            data: "aGVsbG8=".into(),
        })
        .unwrap();
        assert_eq!(
            image
                .as_image()
                .unwrap()
                .source
                .as_ref()
                .unwrap()
                .as_bytes()
                .unwrap()
                .as_ref(),
            b"hello"
        );
        assert!(
            block(Block::Image {
                media_type: "image/svg+xml".into(),
                data: String::new()
            })
            .is_err()
        );
    }
    fn start(decoder: &mut Decoder) {
        decoder
            .consume(ConverseStreamOutput::MessageStart(
                MessageStartEvent::builder()
                    .role(ConversationRole::Assistant)
                    .build()
                    .unwrap(),
            ))
            .unwrap();
    }
    fn tool(decoder: &mut Decoder, chunk: &str) {
        decoder
            .consume(ConverseStreamOutput::ContentBlockStart(
                ContentBlockStartEvent::builder()
                    .content_block_index(0)
                    .start(ContentBlockStart::ToolUse(
                        ToolUseBlockStart::builder()
                            .tool_use_id("call1")
                            .name("read")
                            .build()
                            .unwrap(),
                    ))
                    .build()
                    .unwrap(),
            ))
            .unwrap();
        decoder
            .consume(ConverseStreamOutput::ContentBlockDelta(
                ContentBlockDeltaEvent::builder()
                    .content_block_index(0)
                    .delta(ContentBlockDelta::ToolUse(
                        ToolUseBlockDelta::builder().input(chunk).build().unwrap(),
                    ))
                    .build()
                    .unwrap(),
            ))
            .unwrap();
    }
    fn stop_block(decoder: &mut Decoder) {
        decoder
            .consume(ConverseStreamOutput::ContentBlockStop(
                ContentBlockStopEvent::builder()
                    .content_block_index(0)
                    .build()
                    .unwrap(),
            ))
            .unwrap();
    }
    fn stop(
        decoder: &mut Decoder,
        reason: StopReason,
    ) -> Result<Option<(&'static str, Value)>, Fault> {
        decoder.consume(ConverseStreamOutput::MessageStop(
            MessageStopEvent::builder()
                .stop_reason(reason)
                .build()
                .unwrap(),
        ))
    }
    #[test]
    fn fragmented_tool_requires_block_stop_message_stop_metadata_and_eof() {
        let mut decoder = Decoder::default();
        start(&mut decoder);
        tool(&mut decoder, "{\"path\":");
        decoder
            .consume(ConverseStreamOutput::ContentBlockDelta(
                ContentBlockDeltaEvent::builder()
                    .content_block_index(0)
                    .delta(ContentBlockDelta::ToolUse(
                        ToolUseBlockDelta::builder()
                            .input("\"file\"}")
                            .build()
                            .unwrap(),
                    ))
                    .build()
                    .unwrap(),
            ))
            .unwrap();
        stop_block(&mut decoder);
        stop(&mut decoder, StopReason::ToolUse).unwrap();
        decoder
            .consume(ConverseStreamOutput::Metadata(
                ConverseStreamMetadataEvent::builder()
                    .usage(
                        TokenUsage::builder()
                            .input_tokens(5)
                            .output_tokens(3)
                            .total_tokens(8)
                            .build()
                            .unwrap(),
                    )
                    .build(),
            ))
            .unwrap();
        let reply = decoder
            .finish(&projection::test_target("bedrock-converse-stream"))
            .unwrap();
        assert!(
            matches!(&reply.items[0], Item::ToolCall { arguments, .. } if arguments == "{\"path\":\"file\"}")
        );
        assert_eq!(reply.usage["raw"]["totalTokens"], 8);
    }
    #[test]
    fn truncated_and_unclosed_tools_fail_before_execution() {
        let mut decoder = Decoder::default();
        start(&mut decoder);
        tool(&mut decoder, "{");
        assert!(stop(&mut decoder, StopReason::ToolUse).is_err());
        stop_block(&mut decoder);
        assert!(stop(&mut decoder, StopReason::MaxTokens).is_err());
    }
    fn input() -> ModelInput {
        ModelInput {
            target: None,
            max_output_tokens: None,
            items: vec![Item::Message {
                role: "user".into(),
                content: vec![Block::Text {
                    text: "hello".into(),
                }],
            }],
            tools: vec![],
        }
    }
    #[tokio::test]
    async fn sdk_signs_one_attempt_and_sanitizes_service_errors() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        for bearer in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut target = projection::test_target("bedrock-converse-stream");
            target.compat = json!({ "region": "us-east-1" });
            target.headers.insert("x-fixture".into(), "custom".into());
            target.base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = vec![0; 16384];
                let n = socket.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..n]).to_string();
                let body = "{\"message\":\"secret-token-should-never-escape\"}";
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: \
                             application/json\r\nx-amzn-errortype: \
                             ThrottlingException\r\nRetry-After: 2\r\nContent-Length: \
                             {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                (request, listener)
            });
            let credential = if bearer {
                CredentialReply {
                    base_url: None,
                    available_model_ids: None,
                    catalog_scope: None,
                    api_key: Some("test-bearer".into()),
                    headers: BTreeMap::new(),
                    source: "test".into(),
                }
            } else {
                CredentialReply {
                    base_url: None,
                    available_model_ids: None,
                    catalog_scope: None,
                    api_key: None,
                    headers: BTreeMap::from([(
                        cloud::AWS_PRIVATE_HEADER.into(),
                        json!({
                            "access_key": "test-access",
                            "secret_key": "test-secret",
                            "session_token": "test-session",
                        })
                        .to_string(),
                    )]),
                    source: "test".into(),
                }
            };
            let err = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                request(&target, &credential, &input(), |_, _| Ok(())),
            )
            .await
            .unwrap()
            .unwrap_err();
            assert!(!format!("{err:?}").contains("secret-token"));
            assert_eq!(err.code, "RetryableProviderFailure");
            assert_eq!(err.retry_after_ms, Some(2000));
            let (wire, listener) = server.await.unwrap();
            let lower = wire.to_lowercase();
            assert!(!lower.contains(cloud::AWS_PRIVATE_HEADER));
            assert!(lower.contains("x-fixture: custom"));
            if bearer {
                assert!(lower.contains("authorization: bearer test-bearer"));
            } else {
                assert!(lower.contains("authorization: aws4-hmac-sha256"));
                assert!(lower.contains("x-amz-security-token: test-session"));
                assert!(!lower.contains("test-secret"));
            }
            // A completed single-attempt request cannot leave an already queued retry.
            assert!(listener.into_std().unwrap().accept().is_err());
        }
    }
    #[tokio::test]
    async fn dropping_request_closes_inflight_connection() {
        use tokio::{io::AsyncReadExt, net::TcpListener, sync::oneshot};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut target = projection::test_target("bedrock-converse-stream");
        target.compat = json!({ "region": "us-east-1" });
        target.base_url = format!("http://{}", listener.local_addr().unwrap());
        let (seen_tx, seen_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                    let len: usize = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:").map(str::trim))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if bytes.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            seen_tx.send(()).unwrap();
            socket.read(&mut chunk).await.unwrap()
        });
        let task = tokio::spawn(async move {
            request(
                &target,
                &CredentialReply {
                    base_url: None,
                    available_model_ids: None,
                    catalog_scope: None,
                    api_key: Some("test-bearer".into()),
                    headers: BTreeMap::new(),
                    source: "test".into(),
                },
                &input(),
                |_, _| Ok(()),
            )
            .await
        });
        seen_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read, 0);
    }

    #[test]
    fn adaptive_model_identity_and_effort_are_projected_without_budget() {
        let mut target = projection::test_target("bedrock-converse-stream");
        target.model = "us.anthropic.claude-opus-4-6-v1".into();
        target.capabilities.reasoning = true;
        target.thinking.effective = Some("xhigh".into());
        let fields = thinking_fields(&target, 100).unwrap().unwrap();
        assert_eq!(fields["thinking"]["type"], "adaptive");
        assert_eq!(fields["output_config"]["effort"], "high");
        assert!(fields["thinking"].get("budget_tokens").is_none());
        target.model = "us.anthropic.claude-opus-4-7-v1".into();
        assert_eq!(
            thinking_fields(&target, 100).unwrap().unwrap()["output_config"]["effort"],
            "xhigh"
        );
    }
    #[test]
    fn service_classification_distinguishes_quota_auth_context_and_transient() {
        use aws_sdk_bedrockruntime::{
            error::SdkError, operation::converse_stream::ConverseStreamError,
        };
        // Metadata is inspected for classification but never exposed in Fault text.
        for (code, message, status, expected) in [
            (
                "ServiceQuotaExceededException",
                "private",
                429,
                "ProviderFailure",
            ),
            ("AccessDeniedException", "private", 403, "ProviderFailure"),
            (
                "ValidationException",
                "Input is too long: private",
                400,
                "ContextOverflow",
            ),
            (
                "ThrottlingException",
                "private",
                429,
                "RetryableProviderFailure",
            ),
        ] {
            let meta = aws_smithy_types::error::ErrorMetadata::builder()
                .code(code)
                .message(message)
                .build();
            let error: SdkError<ConverseStreamError, ()> =
                SdkError::service_error(ConverseStreamError::generic(meta), ());
            let fault = sdk_fault(&error, Some(status), None);
            assert_eq!(fault.code, expected);
            assert!(!fault.message.contains("private"));
        }
    }

    #[test]
    fn catalog_max_effort_and_explicit_profile_adaptive_override_are_preserved() {
        let mut target = projection::test_target("bedrock-converse-stream");
        target.model = "anthropic.claude-opus-4-6-v1".into();
        target.thinking.effective = Some("max".into());
        target.compat = json!({ "thinkingLevelMap": { "max": "max" } });
        assert_eq!(
            thinking_fields(&target, 2048).unwrap().unwrap()["output_config"]["effort"],
            "max"
        );
        target.model =
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/example".into();
        target.compat["forceAdaptiveThinking"] = json!(true);
        assert_eq!(
            thinking_fields(&target, 2048).unwrap().unwrap()["thinking"]["type"],
            "adaptive"
        );
    }
}
