use super::*;
use eden_protocol::coding::{Block, Item, ToolDefinition, ToolResult};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

async fn read_request(socket: &mut TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    loop {
        let mut buf = [0; 4096];
        let n = socket.read(&mut buf).await.unwrap();
        assert_ne!(n, 0);
        bytes.extend_from_slice(&buf[..n]);
        if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
            let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
            let len: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_owned)
                })
                .unwrap()
                .parse()
                .unwrap();
            if bytes.len() >= end + 4 + len {
                return (
                    headers,
                    serde_json::from_slice(&bytes[end + 4..end + 4 + len]).unwrap(),
                );
            }
        }
    }
}
fn event(value: Value) -> String {
    format!("data: {value}\r\n\r\n")
}
fn complete(output: Value) -> String {
    event(json!({
        "type": "response.completed",
        "response": {
            "status": "completed",
            "output": output,
            "usage": { "input_tokens": 12, "output_tokens": 5 },
        },
    }))
}
async fn server(status: &str, body: String) -> (String, tokio::task::JoinHandle<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1/responses", listener.local_addr().unwrap());
    let status = status.to_owned();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let received = read_request(&mut socket).await;
        socket
            .write_all(
                format!(
                    concat!(
                        "HTTP/1.1 {status}\r\n",
                        "Content-Type: text/event-stream\r\n",
                        "Content-Length: {}\r\n",
                        "Connection: close\r\n",
                        "\r\n",
                    ),
                    body.len(),
                    status = status,
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        for byte in body.as_bytes() {
            if socket.write_all(&[*byte]).await.is_err() {
                break;
            }
        }
        received
    });
    (endpoint, task)
}
fn input() -> ModelInput {
    ModelInput {
        target: None,
        max_output_tokens: None,
        items: vec![],
        tools: vec![],
    }
}

#[tokio::test]
async fn streams_unicode_and_complete_multiple_calls_and_projects_all_input() {
    let reasoning = json!({
        "type": "reasoning",
        "id": "rs_1",
        "summary": [],
        "encrypted_content": "opaque",
    });
    let body = event(json!({
        "type": "response.output_text.delta",
        "delta": "你好🙂",
        "item_id": "msg_1",
    })) + &event(json!({
        "type": "response.function_call_arguments.delta",
        "delta": "{\"path\":",
        "item_id": "fc_1",
        "output_index": 1,
    })) + &event(json!({
        "type": "response.function_call_arguments.delta",
        "delta": "\"a\"}",
        "item_id": "fc_1",
        "output_index": 1,
    })) + &complete(json!([
        reasoning,
        {
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "你好🙂" }],
        },
        {
            "type": "function_call",
            "call_id": "c1",
            "name": "read",
            "arguments": "{\"path\":\"a\"}",
        },
        {
            "type": "function_call",
            "call_id": "c2",
            "name": "read",
            "arguments": "{\"path\":\"b\"}",
        }
    ]));
    let (endpoint, server) = server("200 OK", body).await;
    let input = ModelInput {
        target: None,
        max_output_tokens: None,
        items: vec![
            Item::Message {
                role: "user".into(),
                content: vec![
                    Block::Text {
                        text: "task".into(),
                    },
                    Block::Image {
                        media_type: "image/png".into(),
                        data: "YWJj".into(),
                    },
                    Block::File {
                        name: "a.pdf".into(),
                        media_type: "application/pdf".into(),
                        data: "ZGVm".into(),
                    },
                ],
            },
            Item::Message {
                role: "assistant".into(),
                content: vec![Block::Text {
                    text: "prior".into(),
                }],
            },
            Item::ProviderState {
                provider: "openai-responses".into(),
                value: reasoning.clone(),
            },
            Item::ToolCall {
                call_id: "old".into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
            Item::ToolResult {
                call_id: "old".into(),
                result: ToolResult {
                    content: vec![],
                    details: serde_json::Value::Null,
                    artifacts: vec![],
                    text: "file".into(),
                    exit_code: Some(0),
                    truncated: false,
                    error: None,
                },
            },
        ],
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
        }],
    };
    let mut deltas = Vec::new();
    let reply = request(
        &endpoint,
        "explicit-model",
        "test-secret",
        &input,
        &RequestOptions::default(),
        |kind, payload| {
            deltas.push((kind.to_owned(), payload));
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(reply.items.len(), 4);
    assert_eq!(
        reply.items[0],
        Item::ProviderState {
            provider: "openai-responses".into(),
            value: reasoning.clone()
        }
    );
    assert_eq!(
        reply.items[2],
        Item::ToolCall {
            call_id: "c1".into(),
            name: "read".into(),
            arguments: "{\"path\":\"a\"}".into()
        }
    );
    assert_eq!(reply.usage["input_tokens"], 12);
    assert_eq!(deltas.len(), 3);
    assert_eq!(deltas[0].1["delta"], "你好🙂");
    let (headers, body) = server.await.unwrap();
    assert!(headers.starts_with("POST /v1/responses HTTP/1.1"));
    assert!(
        headers
            .to_lowercase()
            .contains("authorization: bearer test-secret")
    );
    assert_eq!(body["model"], "explicit-model");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(
        body["input"][0]["content"][1],
        json!({ "type": "input_image", "image_url": "data:image/png;base64,YWJj" })
    );
    assert_eq!(
        body["input"][0]["content"][2],
        json!({
            "type": "input_file",
            "filename": "a.pdf",
            "file_data": "data:application/pdf;base64,ZGVm",
        })
    );
    assert_eq!(body["input"][1]["content"][0]["type"], "output_text");
    assert_eq!(body["input"][2], reasoning);
    assert_eq!(body["input"][3]["call_id"], "old");
    assert_eq!(body["input"][4]["type"], "function_call_output");
    assert_eq!(body["tools"][0]["name"], "read");
    assert_eq!(body["tools"][0]["strict"], false);
}

#[tokio::test]
async fn partial_and_failed_streams_never_become_success_or_leak_server_secrets() {
    for body in [
        event(json!({ "type": "error", "message": "test-secret" })),
        event(json!({
            "type": "response.failed",
            "response": { "error": { "message": "test-secret" } },
        })),
        event(json!({ "type": "response.incomplete", "response": { "status": "incomplete" } })),
        complete(json!([{
            "type": "function_call",
            "call_id": "c",
            "name": "read",
            "arguments": "{",
        }])),
        "data: broken\r\n\r\n".into(),
    ] {
        let (endpoint, server) = server("200 OK", body).await;
        let fault = request(
            &endpoint,
            "explicit-model",
            "test-secret",
            &input(),
            &RequestOptions::default(),
            |_, _| Ok(()),
        )
        .await
        .unwrap_err();
        assert_eq!(fault.code, "ProviderFailure");
        assert!(!fault.to_string().contains("test-secret"));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn http_failure_is_structured_without_echoing_response_body() {
    let (endpoint, server) = server("401 Unauthorized", "test-secret".into()).await;
    let fault = request(
        &endpoint,
        "explicit-model",
        "test-secret",
        &input(),
        &RequestOptions::default(),
        |_, _| Ok(()),
    )
    .await
    .unwrap_err();
    assert_eq!(fault.code, "ProviderFailure");
    assert!(fault.message.contains("401"));
    assert!(!fault.message.contains("test-secret"));
    server.await.unwrap();
}

#[tokio::test]
async fn cancelling_inflight_request_closes_its_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
    let (ready_tx, ready_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket
            .write_all(
                concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "Content-Type: text/event-stream\r\n",
                    "Transfer-Encoding: chunked\r\n",
                    "\r\n",
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let event = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ready\"}\n\n";
        socket
            .write_all(format!("{:x}\r\n{}\r\n", event.len(), event).as_bytes())
            .await
            .unwrap();
        let mut buf = [0];
        match socket.read(&mut buf).await {
            Ok(0) => {}
            // Cancelling HTTP does not promise a graceful TCP FIN. macOS
            // can report ECONNRESET; both establish the connection is closed.
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            other => panic!("cancelled provider connection remained open: {other:?}"),
        }
    });
    let request = tokio::spawn(async move {
        let mut ready_tx = Some(ready_tx);
        request(
            &endpoint,
            "explicit-model",
            "test-secret",
            &input(),
            &RequestOptions::default(),
            move |kind, _| {
                if kind == "model_text_delta"
                    && let Some(sender) = ready_tx.take()
                {
                    sender.send(()).unwrap();
                }
                Ok(())
            },
        )
        .await
    });
    ready_rx.await.unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn sse_preserves_utf8_crlf_multiline_data_and_ignores_comments_at_every_split() {
    let body = concat!(
        ": keepalive\r\n",
        "event: response.output_text.delta\r\n",
        "data: {\r\n",
        "data: \"type\":\"response.output_text.delta\",\"delta\":\"你好🙂\"}\r\n",
        "\r\n",
        "data: [DONE]\r\n",
        "\r\n",
    );
    for split in 0..=body.len() {
        let mut decoder = Sse::default();
        let mut events = decoder.push(&body.as_bytes()[..split]).unwrap();
        events.extend(decoder.push(&body.as_bytes()[split..]).unwrap());
        assert_eq!(
            events,
            vec![
                json!({ "type": "response.output_text.delta", "delta": "你好🙂" }),
                json!({ "type": "eden.done" })
            ]
        );
    }
    let mut decoder = Sse::default();
    let mut events = Vec::new();
    for byte in body.as_bytes() {
        events.extend(decoder.push(&[*byte]).unwrap());
    }
    assert_eq!(events.len(), 2);
}

#[test]
fn sse_rejects_invalid_utf8_and_output_rejects_duplicate_or_partial_calls() {
    assert_eq!(
        Sse::default().push(b"data: \xff\n\n").unwrap_err().code,
        "ProviderFailure"
    );
    let call =
        json!({ "type": "function_call", "call_id": "c", "name": "read", "arguments": "{}" });
    assert!(
        wire::completed(
            &json!({ "status": "completed", "output": [call, call] }),
            Profile::Openai
        )
        .is_err()
    );
    assert!(
        wire::completed(
            &json!({
                "status": "completed",
                "output": [{
                    "type": "function_call",
                    "status": "in_progress",
                    "call_id": "c",
                    "name": "read",
                    "arguments": "{}",
                }],
            }),
            Profile::Openai
        )
        .is_err()
    );
}

#[tokio::test]
async fn output_item_done_is_not_response_completion() {
    let body = event(json!({
        "type": "response.output_item.done",
        "item": {
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "apparently done" }],
        },
    })) + "data: [DONE]\n\n";
    let (endpoint, server) = server("200 OK", body).await;
    assert!(
        request(
            &endpoint,
            "explicit-model",
            "test-secret",
            &input(),
            &RequestOptions::default(),
            |_, _| Ok(())
        )
        .await
        .is_err()
    );
    server.await.unwrap();
}

fn configured(value: Value, env: &[(&str, &str)]) -> Result<Settings, Fault> {
    let config: Config = serde_json::from_value(value).unwrap();
    config.settings_with(|key| {
        env.iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| (*value).to_owned())
    })
}

#[test]
fn profiles_resolve_explicit_config_before_environment_without_implicit_model() {
    let env = [
        ("EDEN_RESPONSES_PROFILE", "deepseek"),
        ("OPENAI_MODEL", "deepseek-flash"),
        ("OPENAI_BASE_URL", "https://api.deepseek.com/"),
        ("EDEN_API_KEY_ENV", "DEEPSEEK_API_KEY"),
        ("DEEPSEEK_API_KEY", "controlled-key"),
    ];
    let settings = configured(json!({}), &env).unwrap();
    assert_eq!(settings.model, "deepseek-flash");
    assert_eq!(settings.endpoint, "https://api.deepseek.com/responses");
    assert_eq!(settings.key, "controlled-key");
    let body = wire::project(&input(), &settings.model, &settings.options).unwrap();
    assert_eq!(body["max_output_tokens"], 393216);
    assert_eq!(body["reasoning"]["effort"], "high");
    assert!(body.get("include").is_none());
    assert!(body.get("store").is_none());
    assert!(body.get("max_tool_calls").is_none());
    let settings = configured(
        json!({
            "profile": "openai",
            "model": "explicit",
            "endpoint": "http://localhost/responses",
            "api_key_env": "LOCAL_KEY",
            "max_output_tokens": 12345,
            "reasoning_effort": "low",
        }),
        &[
            ("EDEN_RESPONSES_PROFILE", "invalid"),
            ("EDEN_API_KEY_ENV", "absent"),
            ("OPENAI_MAX_OUTPUT_TOKENS", "invalid"),
            ("OPENAI_REASONING_EFFORT", "high"),
            ("LOCAL_KEY", "local-key"),
        ],
    )
    .unwrap();
    assert_eq!(settings.model, "explicit");
    assert_eq!(settings.endpoint, "http://localhost/responses");
    assert_eq!(settings.key, "local-key");
    let body = wire::project(&input(), &settings.model, &settings.options).unwrap();
    assert_eq!(body["max_output_tokens"], 12345);
    assert_eq!(body["reasoning"]["effort"], "low");
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["store"], false);
    assert!(
        configured(
            json!({ "profile": "deepseek" }),
            &[("OPENAI_API_KEY", "key")]
        )
        .is_err()
    );
}

#[test]
fn legacy_defaults_and_invalid_settings_remain_explicit() {
    let base = [("OPENAI_MODEL", "legacy"), ("OPENAI_API_KEY", "key")];
    let settings = configured(json!({}), &base).unwrap();
    assert_eq!(settings.endpoint, "https://api.openai.com/v1/responses");
    let body = wire::project(&input(), &settings.model, &settings.options).unwrap();
    assert!(body.get("max_output_tokens").is_none());
    assert!(body.get("reasoning").is_none());
    for extra in [
        ("EDEN_RESPONSES_PROFILE", "unknown"),
        ("OPENAI_MAX_OUTPUT_TOKENS", "oops"),
        ("OPENAI_MAX_OUTPUT_TOKENS", "0"),
        ("OPENAI_REASONING_EFFORT", ""),
    ] {
        let mut env = base.to_vec();
        env.push(extra);
        assert!(configured(json!({}), &env).is_err());
    }
    let settings = configured(
        json!({ "profile": "deepseek" }),
        &[
            ("OPENAI_MODEL", "deepseek-flash"),
            ("OPENAI_API_KEY", "key"),
            ("OPENAI_MAX_OUTPUT_TOKENS", "393216"),
            ("OPENAI_REASONING_EFFORT", "none"),
        ],
    )
    .unwrap();
    let body = wire::project(&input(), &settings.model, &settings.options).unwrap();
    assert_eq!(body["max_output_tokens"], 393216);
    assert_eq!(body["reasoning"]["effort"], "none");
}

#[tokio::test]
async fn deepseek_reasoning_and_tool_result_roundtrip_preserves_images_and_full_output_budget() {
    let reasoning = json!({
        "type": "reasoning",
        "id": "rs_deepseek",
        "summary": [],
        "content": [{ "type": "reasoning_text", "text": "inspect the source" }],
    });
    let body = event(json!({
        "type": "response.reasoning_text.delta",
        "delta": "inspect",
        "item_id": "rs_deepseek",
        "output_index": 0,
    })) + &complete(json!([
        reasoning,
        {
            "type": "function_call",
            "call_id": "read_1",
            "name": "read",
            "arguments": "{\"path\":\"main.rs\"}",
        }
    ]));
    let (endpoint, first_server) = server("200 OK", body).await;
    let settings = configured(
        json!({ "profile": "deepseek", "model": "deepseek-flash", "endpoint": endpoint }),
        &[("OPENAI_API_KEY", "deepseek-key")],
    )
    .unwrap();
    let mut conversation = ModelInput {
        target: None,
        max_output_tokens: None,
        items: vec![Item::Message {
            role: "user".into(),
            content: vec![Block::Image {
                media_type: "image/png".into(),
                data: "YWJj".into(),
            }],
        }],
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "read source".into(),
            parameters: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        }],
    };
    let mut emitted = Vec::new();
    let reply = request(
        &settings.endpoint,
        &settings.model,
        &settings.key,
        &conversation,
        &settings.options,
        |kind, payload| {
            emitted.push((kind.to_owned(), payload));
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(emitted[0].0, "model_reasoning_delta");
    assert_eq!(emitted[0].1["item_id"], "rs_deepseek");
    assert_eq!(
        reply.items[0],
        Item::ProviderState {
            provider: "deepseek-responses".into(),
            value: reasoning.clone()
        }
    );
    let (headers, first_body) = first_server.await.unwrap();
    assert!(
        headers
            .to_lowercase()
            .contains("authorization: bearer deepseek-key")
    );
    assert_eq!(first_body["max_output_tokens"], 393216);
    assert_eq!(
        first_body["input"][0]["content"][0]["image_url"],
        "data:image/png;base64,YWJj"
    );
    // The next provider call receives the exact durable protocol items and correlated tool result.
    conversation.items.extend(reply.items);
    conversation.items.push(Item::ToolResult {
        call_id: "read_1".into(),
        result: ToolResult {
            content: vec![],
            details: serde_json::Value::Null,
            artifacts: vec![],
            text: "fn main() {}".into(),
            exit_code: Some(0),
            truncated: false,
            error: None,
        },
    });
    let (endpoint, second_server) = server(
        "200 OK",
        complete(json!([{
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "done" }],
        }])),
    )
    .await;
    let reply = request(
        &endpoint,
        &settings.model,
        &settings.key,
        &conversation,
        &settings.options,
        |_, _| Ok(()),
    )
    .await
    .unwrap();
    assert!(matches!(&reply.items[0], Item::Message { .. }));
    let (_, second_body) = second_server.await.unwrap();
    assert_eq!(second_body["input"][1], reasoning);
    assert_eq!(second_body["input"][2]["call_id"], "read_1");
    assert_eq!(second_body["input"][3]["call_id"], "read_1");
    assert_eq!(second_body["input"][3]["type"], "function_call_output");
    assert_eq!(second_body["max_output_tokens"], 393216);
    assert!(wire::project(&conversation, "openai", &RequestOptions::default()).is_err());
    let mut legacy = input();
    legacy.items.push(Item::ProviderState {
        provider: "openai-responses".into(),
        value: json!({ "type": "reasoning", "encrypted_content": "opaque" }),
    });
    assert!(wire::project(&legacy, "deepseek-flash", &settings.options).is_err());
}

#[tokio::test]
async fn deepseek_file_input_is_rejected_before_endpoint_or_network_access() {
    let settings = configured(
        json!({ "profile": "deepseek", "model": "deepseek-flash", "endpoint": "invalid URL" }),
        &[("OPENAI_API_KEY", "key")],
    )
    .unwrap();
    let mut input = input();
    input.items.push(Item::Message {
        role: "user".into(),
        content: vec![Block::File {
            name: "task.pdf".into(),
            media_type: "application/pdf".into(),
            data: "YWJj".into(),
        }],
    });
    let fault = request(
        &settings.endpoint,
        &settings.model,
        &settings.key,
        &input,
        &settings.options,
        |_, _| Ok(()),
    )
    .await
    .unwrap_err();
    assert_eq!(
        fault.message,
        "DeepSeek Responses does not support file input; use text or images"
    );
}

#[tokio::test]
async fn deepseek_http_error_returns_first_failure_without_retrying() {
    let (endpoint, server) = server("429 Too Many Requests", "private error detail".into()).await;
    let settings = configured(
        json!({ "profile": "deepseek", "model": "deepseek-flash", "endpoint": endpoint }),
        &[("OPENAI_API_KEY", "key")],
    )
    .unwrap();
    let fault = request(
        &settings.endpoint,
        &settings.model,
        &settings.key,
        &input(),
        &settings.options,
        |_, _| Ok(()),
    )
    .await
    .unwrap_err();
    // This server closes its listener after the first request. A retry would replace
    // this exact HTTP failure with a transport error, or leave the request pending.
    assert_eq!(fault.message, "Responses HTTP status 429");
    let (_, body) = server.await.unwrap();
    assert_eq!(body["max_output_tokens"], 393216);
    assert!(body.get("max_tool_calls").is_none());
}

#[test]
fn request_output_override_is_bounded_and_zero_is_rejected() {
    let options = RequestOptions {
        max_output_tokens: Some(393216),
        ..Default::default()
    };
    let mut input = input();
    for (requested, expected) in [(None, 393216), (Some(13107), 13107), (Some(500000), 393216)] {
        input.max_output_tokens = requested;
        assert_eq!(
            wire::project(&input, "model", &options).unwrap()["max_output_tokens"],
            expected
        );
    }
    input.max_output_tokens = Some(0);
    assert!(wire::project(&input, "model", &options).is_err());
}

#[test]
fn model_limits_do_not_read_credentials_or_require_a_model() {
    let config: Config = serde_json::from_value(json!({ "profile": "deepseek" })).unwrap();
    let limits = config
        .limits_with(|key| {
            assert_ne!(key, "OPENAI_API_KEY");
            None
        })
        .unwrap();
    assert_eq!(limits.context_window, 1_048_576);
    assert_eq!(limits.max_output_tokens, 393216);
    let config: Config = serde_json::from_value(json!({
        "context_window": 90000,
        "max_output_tokens": 8000,
    }))
    .unwrap();
    let limits = config.limits_with(|_| None).unwrap();
    assert_eq!(limits.context_window, 90000);
    assert_eq!(limits.max_output_tokens, 8000);
    assert_eq!(
        Config::default()
            .limits_with(|_| None)
            .unwrap()
            .context_window,
        0
    );
    assert!(descriptor().provides.iter().any(|role| role == MODEL_INFO));
    assert!(create(json!({ "profile": "deepseek" })).is_ok());
}

#[tokio::test]
async fn provider_failures_classify_without_exposing_private_details() {
    for (status, code, expected) in [
        (
            "429 Too Many Requests",
            "rate_limit_exceeded",
            "RetryableProviderFailure",
        ),
        (
            "503 Service Unavailable",
            "server_error",
            "RetryableProviderFailure",
        ),
        (
            "429 Too Many Requests",
            "insufficient_quota",
            "ProviderFailure",
        ),
        (
            "429 Too Many Requests",
            "billing_hard_limit_reached",
            "ProviderFailure",
        ),
        ("401 Unauthorized", "invalid_api_key", "ProviderFailure"),
        (
            "400 Bad Request",
            "context_length_exceeded",
            "ContextOverflow",
        ),
        (
            "422 Unprocessable Entity",
            "invalid_parameter",
            "ProviderFailure",
        ),
    ] {
        let (endpoint, server) = server(
            status,
            json!({ "error": { "code": code, "message": "test-secret" } }).to_string(),
        )
        .await;
        let fault = request(
            &endpoint,
            "model",
            "test-secret",
            &input(),
            &RequestOptions::default(),
            |_, _| Ok(()),
        )
        .await
        .unwrap_err();
        assert_eq!(fault.code, expected, "{status}: {code}");
        assert!(!fault.to_string().contains("test-secret"));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn dropped_stream_is_retryable_and_incomplete_output_never_returns_calls() {
    for (body, expected) in [
        (
            event(json!({ "type": "response.output_text.delta", "delta": "partial" })),
            "RetryableProviderFailure",
        ),
        (
            event(json!({
                "type": "response.failed",
                "response": {
                    "error": { "code": "context_length_exceeded", "message": "test-secret" },
                },
            })),
            "ContextOverflow",
        ),
        (
            event(json!({
                "type": "response.incomplete",
                "response": {
                    "status": "incomplete",
                    "incomplete_details": { "reason": "max_output_tokens" },
                    "usage": { "output_tokens": 100 },
                    "output": [{
                        "type": "function_call",
                        "call_id": "c1",
                        "name": "write",
                        "arguments": "{}",
                    }],
                },
            })),
            "RecoverableLength",
        ),
        (
            event(json!({
                "type": "response.incomplete",
                "response": {
                    "status": "incomplete",
                    "incomplete_details": { "reason": "max_output_tokens" },
                    "usage": { "output_tokens": 1000 },
                },
            })),
            "ProviderFailure",
        ),
    ] {
        let (endpoint, server) = server("200 OK", body).await;
        let options = RequestOptions {
            max_output_tokens: Some(1000),
            ..Default::default()
        };
        let fault = request(
            &endpoint,
            "model",
            "test-secret",
            &input(),
            &options,
            |_, _| Ok(()),
        )
        .await
        .unwrap_err();
        assert_eq!(fault.code, expected);
        assert!(!fault.to_string().contains("test-secret"));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn incomplete_summary_is_terminal_even_when_usage_is_below_its_allowance() {
    let body = event(json!({
        "type": "response.incomplete",
        "response": {
            "incomplete_details": { "reason": "max_output_tokens" },
            "usage": { "output_tokens": 10 },
        },
    }));
    let (endpoint, server) = server("200 OK", body).await;
    let mut input = input();
    input.max_output_tokens = Some(13107);
    let options = RequestOptions {
        max_output_tokens: Some(393216),
        ..Default::default()
    };
    assert_eq!(
        request(&endpoint, "model", "key", &input, &options, |_, _| Ok(()))
            .await
            .unwrap_err()
            .code,
        "ProviderFailure"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn connection_dropped_before_response_is_retryable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket.shutdown().await.unwrap();
    });
    let fault = request(
        &endpoint,
        "model",
        "test-secret",
        &input(),
        &RequestOptions::default(),
        |_, _| Ok(()),
    )
    .await
    .unwrap_err();
    assert_eq!(fault.code, "RetryableProviderFailure");
    assert!(!fault.to_string().contains("test-secret"));
    server.await.unwrap();
}

#[test]
fn explicit_context_errors_and_quota_messages_are_classified_without_echoing_them() {
    for (status, message, expected) in [
        (
            400,
            "This model's maximum context length is 1048576 tokens. Your messages resulted in too many tokens: test-secret",
            "ContextOverflow",
        ),
        (
            429,
            "You exceeded your current quota, please check your plan and billing details. test-secret",
            "ProviderFailure",
        ),
        (
            400,
            "invalid parameter containing test-secret",
            "ProviderFailure",
        ),
    ] {
        let fault = wire::provider_fault(
            Some(status),
            &json!({ "error": { "type": "invalid_request_error", "message": message } }),
        );
        assert_eq!(fault.code, expected);
        assert!(!fault.to_string().contains("test-secret"));
    }
}

#[test]
fn image_tool_output_is_native_content_with_metadata_and_old_text_is_compatible() {
    let legacy = json!({ "text": "old", "exit_code": null, "truncated": false, "error": null });
    let mut result: ToolResult = serde_json::from_value(legacy.clone()).unwrap();
    assert!(result.content.is_empty());
    assert_eq!(serde_json::to_value(&result).unwrap(), legacy);
    result.content.push(Block::Image {
        media_type: "image/png".into(),
        data: "YWJj".into(),
    });
    result.details = json!({ "kind": "image" });
    let input = ModelInput {
        target: None,
        max_output_tokens: None,
        tools: vec![],
        items: vec![Item::ToolResult {
            call_id: "image-1".into(),
            result,
        }],
    };
    let body = wire::project(&input, "controlled", &RequestOptions::default()).unwrap();
    let output = &body["input"][0]["output"];
    assert_eq!(output[1]["type"], "input_image");
    assert_eq!(output[1]["image_url"], "data:image/png;base64,YWJj");
    let metadata: Value = serde_json::from_str(output[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(metadata["details"]["kind"], "image");
    assert!(!metadata.to_string().contains("YWJj"));
}

#[tokio::test]
async fn frozen_chat_route_sends_private_auth_and_waits_for_done_usage() {
    let stream = event(json!({
        "choices": [{ "index": 0, "delta": { "content": "hello" }, "finish_reason": null }],
    })) + &event(
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
    ) + &event(json!({
        "choices": [],
        "usage": { "prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6 },
    })) + "data: [DONE]\n\n";
    let (url, received) = server("200 OK", stream).await;
    let mut target = projection::test_target("openai-completions");
    target.base_url = url.trim_end_matches("/responses").into();
    let credential = eden_protocol::models::CredentialReply {
        api_key: Some("PRIVATE_CANARY".into()),
        source: "test".into(),
        headers: Default::default(),
    };
    let reply = targeted::request(&target, &credential, &input(), |_, _| Ok(()))
        .await
        .unwrap();
    assert_eq!(reply.usage["total_tokens"], 6);
    let (headers, body) = received.await.unwrap();
    assert!(headers.starts_with("POST /v1/chat/completions "));
    assert!(
        headers
            .to_lowercase()
            .contains("authorization: bearer private_canary")
    );
    assert_eq!(body["model"], target.model);
    assert!(
        !serde_json::to_string(&reply)
            .unwrap()
            .contains("PRIVATE_CANARY")
    );
}
#[tokio::test]
async fn frozen_anthropic_route_has_version_and_key_and_stream_completion() {
    let stream = event(json!({
        "type": "message_start",
        "message": { "usage": { "input_tokens": 4, "output_tokens": 0 } },
    })) + &event(json!({
        "type": "content_block_start",
        "index": 0,
        "content_block": { "type": "text", "text": "" },
    })) + &event(json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": { "type": "text_delta", "text": "hello" },
    })) + &event(json!({ "type": "content_block_stop", "index": 0 }))
        + &event(json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn" },
            "usage": { "output_tokens": 2 },
        }))
        + &event(json!({ "type": "message_stop" }));
    let (url, received) = server("200 OK", stream).await;
    let mut target = projection::test_target("anthropic-messages");
    target.base_url = url.trim_end_matches("/v1/responses").into();
    let credential = eden_protocol::models::CredentialReply {
        api_key: Some("PRIVATE_CANARY".into()),
        source: "test".into(),
        headers: Default::default(),
    };
    let reply = targeted::request(&target, &credential, &input(), |_, _| Ok(()))
        .await
        .unwrap();
    assert_eq!(reply.usage["total_tokens"], 6);
    let (headers, _) = received.await.unwrap();
    assert!(headers.starts_with("POST /v1/messages "));
    assert!(headers.to_lowercase().contains("x-api-key: private_canary"));
    assert!(headers.contains("anthropic-version: 2023-06-01"));
}
#[tokio::test]
async fn chat_finish_reason_without_done_does_not_execute_partial_tool() {
    let stream = event(json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call",
                    "type": "function",
                    "function": { "name": "write", "arguments": "{" },
                }],
            },
            "finish_reason": "tool_calls",
        }],
    }));
    let (url, received) = server("200 OK", stream).await;
    let mut target = projection::test_target("openai-completions");
    target.base_url = url.trim_end_matches("/responses").into();
    let credential = eden_protocol::models::CredentialReply {
        api_key: Some("test".into()),
        source: "test".into(),
        headers: Default::default(),
    };
    let error = targeted::request(&target, &credential, &input(), |_, _| Ok(()))
        .await
        .unwrap_err();
    assert_eq!(error.code, "RetryableProviderFailure");
    received.await.unwrap();
}

#[test]
fn managed_anthropic_effort_preserves_history_and_requested_active_level() {
    let mut target = projection::test_target("anthropic-messages");
    target.compat = json!({ "supportsMidConvoEffort": true });
    target.thinking.effective = Some("low".into());
    let state = projection::state(
        &target,
        json!({ "type": "thinking", "thinking": "old", "signature": "signature" }),
    );
    target.thinking.effective = Some("medium".into());
    let mut input = input();
    input.items.push(state);
    let body = anthropic::project(&input, &target).unwrap();
    assert_eq!(body["messages"][0]["output_config"]["effort"], "low");
    assert_eq!(
        body["messages"].as_array().unwrap().last().unwrap()["output_config"]["effort"],
        "medium"
    );
    assert_eq!(body["output_config"]["effort"], "high");
}

#[tokio::test]
async fn nonreasoning_responses_omits_reasoning_despite_requested_thinking() {
    let (url, received) = server(
        "200 OK",
        complete(json!([{
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "ok" }],
        }])),
    )
    .await;
    let mut target = projection::test_target("openai-responses");
    target.base_url = url;
    target.capabilities.reasoning = false;
    target.thinking.requested = Some("high".into());
    target.thinking.effective = Some("off".into());
    let credentials = eden_protocol::models::CredentialReply {
        api_key: Some("test".into()),
        source: "test".into(),
        headers: Default::default(),
    };
    targeted::request(&target, &credentials, &input(), |_, _| Ok(()))
        .await
        .unwrap();
    let (_, body) = received.await.unwrap();
    assert!(body.get("reasoning").is_none());
}

#[tokio::test]
async fn g2_http_protocols_stream_text_with_private_auth_and_frozen_routes() {
    for api in [
        "google-generative-ai",
        "google-vertex",
        "azure-openai-responses",
        "mistral-conversations",
    ] {
        let stream = match api {
            "google-generative-ai" | "google-vertex" => event(json!({
                "candidates": [{
                    "content": { "parts": [{ "text": "你好" }] },
                    "finishReason": "STOP",
                }],
                "usageMetadata": {
                    "promptTokenCount": 4,
                    "candidatesTokenCount": 2,
                    "totalTokenCount": 6,
                },
            })),
            "mistral-conversations" => {
                event(json!({
                    "choices": [{
                        "index": 0,
                        "delta": { "content": [{ "type": "text", "text": "你好" }] },
                        "finish_reason": "stop",
                    }],
                    "usage": { "prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6 },
                })) + "data: [DONE]\n\n"
            }
            _ => complete(json!([{
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "你好" }],
            }])),
        };
        let (url, received) = server("200 OK", stream).await;
        let mut target = projection::test_target(api);
        target.base_url = url.trim_end_matches("/responses").into();
        target.compat = json!({ "deployment": "deployed" });
        let credentials = eden_protocol::models::CredentialReply {
            api_key: Some("G2_SECRET".into()),
            headers: Default::default(),
            source: "test".into(),
        };
        let reply = targeted::request(&target, &credentials, &input(), |_, _| Ok(()))
            .await
            .unwrap();
        assert!(
            serde_json::to_string(&reply.items)
                .unwrap()
                .contains("你好")
        );
        let (headers, body) = received.await.unwrap();
        let lower = headers.to_lowercase();
        match api {
            "google-generative-ai" | "google-vertex" => {
                assert!(lower.contains("x-goog-api-key: g2_secret"));
                assert!(headers.contains(":streamGenerateContent?alt=sse"));
                assert_eq!(body["generationConfig"]["maxOutputTokens"], 2048);
            }
            "azure-openai-responses" => {
                assert!(lower.contains("api-key: g2_secret"));
                assert_eq!(body["model"], "deployed");
            }
            _ => assert!(lower.contains("authorization: bearer g2_secret")),
        }
        assert!(!serde_json::to_string(&reply).unwrap().contains("G2_SECRET"));
    }
}
#[tokio::test]
async fn g2_truncated_streams_and_http_errors_never_return_tool_calls() {
    for api in [
        "google-generative-ai",
        "google-vertex",
        "azure-openai-responses",
        "mistral-conversations",
    ] {
        let (url, received) = server(
            "429 Too Many Requests",
            "{\"error\":{\"message\":\"G2_SECRET\"}}".into(),
        )
        .await;
        let mut target = projection::test_target(api);
        target.base_url = url.trim_end_matches("/responses").into();
        let credentials = eden_protocol::models::CredentialReply {
            api_key: Some("G2_SECRET".into()),
            headers: Default::default(),
            source: "test".into(),
        };
        let error = targeted::request(&target, &credentials, &input(), |_, _| Ok(()))
            .await
            .unwrap_err();
        assert_eq!(error.code, "RetryableProviderFailure");
        assert!(!format!("{error:?}").contains("G2_SECRET"));
        received.await.unwrap();
        let stream = if api.starts_with("google") {
            event(json!({
                "candidates": [{
                    "content": {
                        "parts": [{ "functionCall": { "name": "write", "args": { "path": "x" } } }],
                    },
                }],
            }))
        } else {
            event(json!({ "type": "response.function_call_arguments.delta", "delta": "{" }))
        };
        let (url, received) = server("200 OK", stream).await;
        target.base_url = url.trim_end_matches("/responses").into();
        assert!(
            targeted::request(&target, &credentials, &input(), |_, _| Ok(()))
                .await
                .is_err()
        );
        received.await.unwrap();
    }
}
#[tokio::test]
async fn g2_cancellation_closes_each_http_protocol_connection() {
    for api in [
        "google-generative-ai",
        "google-vertex",
        "azure-openai-responses",
        "mistral-conversations",
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut target = projection::test_target(api);
        target.base_url = format!("http://{}", listener.local_addr().unwrap());
        let (ready, wait) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
            ready.send(()).unwrap();
            let mut byte = [0];
            match socket.read(&mut byte).await {
                Ok(0) | Err(_) => (),
                other => panic!("connection not closed: {other:?}"),
            }
        });
        let request = tokio::spawn(async move {
            let c = eden_protocol::models::CredentialReply {
                api_key: Some("test".into()),
                headers: Default::default(),
                source: "test".into(),
            };
            targeted::request(&target, &c, &input(), |_, _| Ok(())).await
        });
        wait.await.unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn cloudflare_gateway_separates_gateway_and_upstream_authentication() {
    let stream = event(json!({
        "choices": [{ "index": 0, "delta": { "content": "ok" }, "finish_reason": "stop" }],
    })) + "data: [DONE]\n\n";
    let (url, received) = server("200 OK", stream).await;
    let mut t = projection::test_target("openai-completions");
    t.provider = "cloudflare-ai-gateway".into();
    t.base_url = url.trim_end_matches("/responses").into();
    let c = eden_protocol::models::CredentialReply {
        api_key: Some("gateway-key".into()),
        headers: std::collections::BTreeMap::from([(
            "Authorization".into(),
            "Bearer upstream-key".into(),
        )]),
        source: "test".into(),
    };
    targeted::request(&t, &c, &input(), |_, _| Ok(()))
        .await
        .unwrap();
    let (headers, _) = received.await.unwrap();
    let headers = headers.to_lowercase();
    assert!(headers.contains("cf-aig-authorization: bearer gateway-key"));
    assert!(headers.contains("authorization: bearer upstream-key"));
}
#[tokio::test]
async fn opencode_google_route_uses_gemini_protocol_from_model_identity() {
    let (url, received) = server(
        "200 OK",
        event(json!({
            "candidates": [{
                "content": { "parts": [{ "text": "gateway" }] },
                "finishReason": "STOP",
            }],
        })),
    )
    .await;
    let mut t = projection::test_target("google-generative-ai");
    t.provider = "opencode".into();
    t.base_url = url.trim_end_matches("/responses").into();
    let c = eden_protocol::models::CredentialReply {
        api_key: Some("gateway-key".into()),
        headers: Default::default(),
        source: "test".into(),
    };
    targeted::request(&t, &c, &input(), |_, _| Ok(()))
        .await
        .unwrap();
    let (headers, body) = received.await.unwrap();
    assert!(headers.starts_with("POST /v1/models/test:streamGenerateContent?alt=sse"));
    assert!(body.get("contents").is_some());
    assert!(body.get("messages").is_none());
}
