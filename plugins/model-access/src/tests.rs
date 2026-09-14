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
    event(
        json!({"type":"response.completed","response":{"status":"completed","output":output,"usage":{"input_tokens":12,"output_tokens":5}}}),
    )
}
async fn server(status: &str, body: String) -> (String, tokio::task::JoinHandle<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1/responses", listener.local_addr().unwrap());
    let status = status.to_owned();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let received = read_request(&mut socket).await;
        socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
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
        items: vec![],
        tools: vec![],
    }
}

#[tokio::test]
async fn streams_unicode_and_complete_multiple_calls_and_projects_all_input() {
    let reasoning =
        json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque"});
    let body = event(
        json!({"type":"response.output_text.delta","delta":"你好🙂","item_id":"msg_1"}),
    ) + &event(
        json!({"type":"response.function_call_arguments.delta","delta":"{\"path\":","item_id":"fc_1","output_index":1}),
    ) + &event(
        json!({"type":"response.function_call_arguments.delta","delta":"\"a\"}","item_id":"fc_1","output_index":1}),
    ) + &complete(
        json!([reasoning,{"type":"message","role":"assistant","content":[{"type":"output_text","text":"你好🙂"}]},{"type":"function_call","call_id":"c1","name":"read","arguments":"{\"path\":\"a\"}"},{"type":"function_call","call_id":"c2","name":"read","arguments":"{\"path\":\"b\"}"}]),
    );
    let (endpoint, server) = server("200 OK", body).await;
    let input = ModelInput {
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
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        }],
    };
    let mut deltas = Vec::new();
    let reply = request(
        &endpoint,
        "explicit-model",
        "test-secret",
        &input,
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
        json!({"type":"input_image","image_url":"data:image/png;base64,YWJj"})
    );
    assert_eq!(
        body["input"][0]["content"][2],
        json!({"type":"input_file","filename":"a.pdf","file_data":"data:application/pdf;base64,ZGVm"})
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
        event(json!({"type":"response.output_text.delta","delta":"partial"})),
        event(json!({"type":"error","message":"test-secret"})),
        event(json!({"type":"response.failed","response":{"error":{"message":"test-secret"}}})),
        event(json!({"type":"response.incomplete","response":{"status":"incomplete"}})),
        complete(json!([{"type":"function_call","call_id":"c","name":"read","arguments":"{"}])),
        "data: broken\r\n\r\n".into(),
    ] {
        let (endpoint, server) = server("200 OK", body).await;
        let fault = request(
            &endpoint,
            "explicit-model",
            "test-secret",
            &input(),
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
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        ready_tx.send(()).unwrap();
        let mut buf = [0];
        assert_eq!(socket.read(&mut buf).await.unwrap(), 0);
    });
    let request = tokio::spawn(async move {
        request(
            &endpoint,
            "explicit-model",
            "test-secret",
            &input(),
            |_, _| Ok(()),
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
    let body = ": keepalive\r\nevent: response.output_text.delta\r\ndata: {\r\ndata: \"type\":\"response.output_text.delta\",\"delta\":\"你好🙂\"}\r\n\r\ndata: [DONE]\r\n\r\n";
    for split in 0..=body.len() {
        let mut decoder = Sse::default();
        let mut events = decoder.push(&body.as_bytes()[..split]).unwrap();
        events.extend(decoder.push(&body.as_bytes()[split..]).unwrap());
        assert_eq!(
            events,
            vec![json!({"type":"response.output_text.delta","delta":"你好🙂"})]
        );
    }
    let mut decoder = Sse::default();
    let mut events = Vec::new();
    for byte in body.as_bytes() {
        events.extend(decoder.push(&[*byte]).unwrap());
    }
    assert_eq!(events.len(), 1);
}

#[test]
fn sse_rejects_invalid_utf8_and_output_rejects_duplicate_or_partial_calls() {
    assert_eq!(
        Sse::default().push(b"data: \xff\n\n").unwrap_err().code,
        "ProviderFailure"
    );
    let call = json!({"type":"function_call","call_id":"c","name":"read","arguments":"{}"});
    assert!(wire::completed(&json!({"status":"completed","output":[call,call]})).is_err());
    assert!(wire::completed(&json!({"status":"completed","output":[{"type":"function_call","status":"in_progress","call_id":"c","name":"read","arguments":"{}"}]})).is_err());
}

#[tokio::test]
async fn output_item_done_is_not_response_completion() {
    let body = event(
        json!({"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"apparently done"}]}}),
    ) + "data: [DONE]\n\n";
    let (endpoint, server) = server("200 OK", body).await;
    assert!(
        request(
            &endpoint,
            "explicit-model",
            "test-secret",
            &input(),
            |_, _| Ok(())
        )
        .await
        .is_err()
    );
    server.await.unwrap();
}
