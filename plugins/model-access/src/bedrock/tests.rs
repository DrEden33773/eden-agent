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
fn stop(decoder: &mut Decoder, reason: StopReason) -> Result<Option<(&'static str, Value)>, Fault> {
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
        matches!(&reply.items[0],Item::ToolCall{arguments,..} if arguments=="{\"path\":\"file\"}")
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
            socket.write_all(format!("HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nx-amzn-errortype: ThrottlingException\r\nRetry-After: 2\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
            (request, listener)
        });
        let credential = if bearer {
            CredentialReply {
                api_key: Some("test-bearer".into()),
                headers: BTreeMap::new(),
                source: "test".into(),
            }
        } else {
            CredentialReply {
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
