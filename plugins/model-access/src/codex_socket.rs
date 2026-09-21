//! A request owns its WebSocket; cancellation drops the socket before loop cleanup.
use crate::{RequestOptions, projection, subscription, usage, wire};
use eden_protocol::{
    Fault,
    coding::{Item, ModelInput, ModelReply},
    models::{CredentialReply, ModelTarget},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

pub(crate) async fn request(
    target: &ModelTarget,
    credential: &CredentialReply,
    input: &ModelInput,
    options: &RequestOptions,
    body: &Value,
    url: &reqwest::Url,
    emit: &mut impl FnMut(&str, Value) -> Result<(), Fault>,
) -> Result<Option<ModelReply>, Fault> {
    let transport = target.compat["transport"].as_str().unwrap_or("auto");
    if transport == "sse" {
        return Ok(None);
    }
    if !matches!(transport, "auto" | "websocket") {
        return Err(wire::failure("unsupported Codex transport"));
    }
    let mut url = url.clone();
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|_| wire::failure("invalid Codex WebSocket endpoint"))?;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| wire::failure("invalid Codex WebSocket request"))?;
    let mut headers = subscription::headers(target, input);
    headers.extend(target.headers.clone());
    if let Some(key) = &credential.api_key {
        headers.insert("Authorization".into(), format!("Bearer {key}"));
    }
    headers.extend(credential.headers.clone());
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|_| wire::failure("cannot obtain Codex request identity"))?;
    random[6] = (random[6] & 0x0f) | 0x40;
    random[8] = (random[8] & 0x3f) | 0x80;
    let hex = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let request_id = format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    headers.insert("x-client-request-id".into(), request_id.clone());
    headers.insert("session-id".into(), request_id);

    headers.insert(
        "OpenAI-Beta".into(),
        "responses_websockets=2026-02-06".into(),
    );
    for (name, value) in headers {
        if name.to_ascii_lowercase().starts_with("x-eden-private-") {
            return Err(wire::failure("private credential cannot be sent to Codex"));
        }
        request.headers_mut().insert(
            name.parse::<tokio_tungstenite::tungstenite::http::HeaderName>()
                .map_err(|_| wire::failure("invalid Codex header name"))?,
            value
                .parse()
                .map_err(|_| wire::failure("invalid Codex header value"))?,
        );
    }
    let connection =
        tokio::time::timeout(std::time::Duration::from_secs(10), connect_async(request)).await;
    let mut socket = match connection {
        Ok(Ok((socket, _))) => socket,
        _ if transport == "auto" => return Ok(None),
        _ => return Err(wire::failure("Codex WebSocket connection failed")),
    };
    let mut payload = body.clone();
    payload["type"] = json!("response.create");
    if let Some(object) = payload.as_object_mut() {
        object.remove("stream");
    }
    if socket
        .send(Message::Text(payload.to_string().into()))
        .await
        .is_err()
    {
        if transport == "auto" {
            return Ok(None);
        }
        return Err(wire::failure("Codex WebSocket send failed"));
    }
    let mut started = false;
    let mut responses = wire::ResponseOutput::default();
    loop {
        let frame = socket.next().await;
        let event = match frame {
            Some(Ok(Message::Text(text))) => serde_json::from_str::<Value>(&text)
                .map_err(|_| wire::failure("invalid Codex WebSocket event"))?,
            Some(Ok(Message::Ping(data))) => {
                socket
                    .send(Message::Pong(data))
                    .await
                    .map_err(|_| wire::failure("Codex WebSocket ping failed"))?;
                continue;
            }
            Some(Ok(Message::Pong(_))) => continue,
            _ if !started && transport == "auto" => return Ok(None),
            _ => {
                return Err(Fault::new(
                    "RetryableProviderFailure",
                    "model-access",
                    "Codex WebSocket ended before completion",
                ));
            }
        };
        started = true;
        responses.observe(&event)?;
        match event["type"].as_str() {
            Some("response.completed" | "response.done") => {
                let mut reply = responses.complete(&event["response"], options.profile)?;
                for item in &mut reply.items {
                    if let Item::ProviderState { value, .. } = item {
                        *item = projection::state(target, value.clone());
                    }
                }
                let mut accounting_target = target.clone();
                accounting_target.api = "openai-responses".into();
                reply.usage = usage::normalize(&reply.usage, &accounting_target, Some("stop"));
                // The socket is dropped with this request, never kept in a hidden session cache.
                return Ok(Some(reply));
            }
            Some("response.incomplete") => {
                return Err(wire::incomplete(&event["response"], input, options));
            }
            Some("response.failed" | "error") => return Err(wire::provider_fault(None, &event)),
            Some("response.output_text.delta" | "response.refusal.delta") => {
                emit("model_text_delta", event)?
            }
            Some("response.reasoning_text.delta" | "response.reasoning_summary_text.delta") => {
                emit("model_reasoning_delta", event)?
            }
            Some("response.function_call_arguments.delta") => emit("model_tool_delta", event)?,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    fn fixture(base: String, transport: &str) -> (ModelTarget, CredentialReply, ModelInput) {
        let mut target = projection::test_target("openai-codex-responses");
        target.base_url = base;
        target.compat = json!({ "transport": transport });
        let credential = CredentialReply {
            api_key: Some("token".into()),
            headers: Default::default(),
            source: "test".into(),
            base_url: None,
            available_model_ids: None,
            catalog_scope: None,
        };
        let input = ModelInput {
            target: None,
            max_output_tokens: None,
            items: vec![],
            tools: vec![],
        };
        (target, credential, input)
    }
    #[tokio::test]
    async fn websocket_sends_response_create_and_releases_complete_tools() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (target, credential, input) = fixture(
            format!("http://{}", listener.local_addr().unwrap()),
            "websocket",
        );
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let message = ws.next().await.unwrap().unwrap();
            let body: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            assert_eq!(body["type"], "response.create");
            assert!(body.get("stream").is_none());
            ws.send(Message::Text(
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "type": "function_call",
                        "call_id": "c",
                        "name": "read",
                        "arguments": "{}",
                    },
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            ws.send(Message::Text(
                json!({
                    "type": "response.done",
                    "response": { "status": "completed", "output": [], "usage": {} },
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            assert!(ws.next().await.is_none_or(|result| result.is_err()));
        });
        let reply = crate::targeted::request(&target, &credential, &input, |_, _| Ok(()))
            .await
            .unwrap();
        assert!(matches!(&reply.items[0],Item::ToolCall{call_id,..} if call_id=="c"));
        server.await.unwrap();
    }
    #[tokio::test]
    async fn cancellation_closes_owned_websocket_without_background_reader() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (target, credential, input) = fixture(
            format!("http://{}", listener.local_addr().unwrap()),
            "websocket",
        );
        let (ready, started) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            ws.next().await.unwrap().unwrap();
            ready.send(()).unwrap();
            assert!(ws.next().await.is_none_or(|result| result.is_err()));
        });
        let request = tokio::spawn(async move {
            crate::targeted::request(&target, &credential, &input, |_, _| Ok(())).await
        });
        started.await.unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        server.await.unwrap();
    }
    #[tokio::test]
    async fn auto_does_not_fallback_after_first_server_event() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (target, credential, input) =
            fixture(format!("http://{}", listener.local_addr().unwrap()), "auto");
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            ws.next().await.unwrap().unwrap();
            ws.send(Message::Text(
                json!({ "type": "response.created" }).to_string().into(),
            ))
            .await
            .unwrap();
            ws.close(None).await.unwrap();
        });
        let error = crate::targeted::request(&target, &credential, &input, |_, _| Ok(()))
            .await
            .unwrap_err();
        assert_eq!(error.code, "RetryableProviderFailure");
        server.await.unwrap();
    }
    #[tokio::test]
    async fn auto_falls_back_to_sse_when_upgrade_fails_before_events() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (target, credential, input) =
            fixture(format!("http://{}", listener.local_addr().unwrap()), "auto");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 8192];
            let n = socket.read(&mut bytes).await.unwrap();
            assert!(
                String::from_utf8_lossy(&bytes[..n])
                    .to_ascii_lowercase()
                    .contains("upgrade: websocket")
            );
            socket.write_all(b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            drop(socket);
            let (mut socket, _) = listener.accept().await.unwrap();
            let n = socket.read(&mut bytes).await.unwrap();
            assert!(String::from_utf8_lossy(&bytes[..n]).starts_with("POST "));
            let event = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello\"}]}],\"usage\":{}}}\n\n";
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{event}",event.len()).as_bytes()).await.unwrap();
        });
        crate::targeted::request(&target, &credential, &input, |_, _| Ok(()))
            .await
            .unwrap();
        server.await.unwrap();
    }
}
