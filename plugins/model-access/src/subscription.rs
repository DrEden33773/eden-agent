//! Subscription adapters keep account routing private and reuse owned wire decoders.
#[cfg(test)]
use crate::projection;
use crate::wire;
use eden_protocol::{
    Fault,
    coding::{Block, Item, ModelInput},
    models::ModelTarget,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(crate) fn codex_body(body: &mut Value) {
    let mut instructions = Vec::new();
    if let Some(items) = body["input"].as_array_mut() {
        items.retain(|item| {
            if matches!(item["role"].as_str(), Some("system" | "developer")) {
                if let Some(content) = item["content"].as_array() {
                    instructions.extend(
                        content
                            .iter()
                            .filter_map(|b| b["text"].as_str())
                            .map(str::to_owned),
                    );
                }
                false
            } else {
                true
            }
        });
    }
    body["instructions"] = json!(if instructions.is_empty() {
        "You are a helpful assistant.".into()
    } else {
        instructions.join("\n\n")
    });
    body["store"] = json!(false);
    body["text"] = json!({ "verbosity": "low" });
    body["include"] = json!(["reasoning.encrypted_content"]);
    body["parallel_tool_calls"] = json!(true);
    if let Some(object) = body.as_object_mut() {
        object.remove("max_output_tokens");
    }
}

pub(crate) fn headers(target: &ModelTarget, input: &ModelInput) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    if target.api == "openai-codex-responses" {
        headers.insert("OpenAI-Beta".into(), "responses=experimental".into());
        headers.insert("originator".into(), "eden-agent".into());
        headers.insert("User-Agent".into(), "eden-agent".into());
    }
    if target.provider == "github-copilot" {
        headers.insert("User-Agent".into(), "eden-agent".into());
        headers.insert("Openai-Intent".into(), "conversation-edits".into());
        let initiator = match input.items.last() {
            None => "user",
            Some(Item::Message { role, .. }) if role == "user" => "user",
            _ => "agent",
        };
        headers.insert("X-Initiator".into(), initiator.into());
        if input.items.iter().any(|item| match item {
            Item::Message { content, .. } => {
                content.iter().any(|b| matches!(b, Block::Image { .. }))
            }
            Item::ToolResult { result, .. } => result
                .content
                .iter()
                .any(|b| matches!(b, Block::Image { .. })),
            _ => false,
        }) {
            headers.insert("Copilot-Vision-Request".into(), "true".into());
        }
    }
    headers
}

pub(crate) fn radius_models(config: &Value) -> Result<Vec<Value>, Fault> {
    let base = config["baseUrl"]
        .as_str()
        .ok_or_else(|| wire::failure("invalid Radius catalog endpoint"))?;
    let url =
        reqwest::Url::parse(base).map_err(|_| wire::failure("invalid Radius catalog endpoint"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(wire::failure("invalid Radius catalog endpoint"));
    }
    config["models"]
        .as_array()
        .ok_or_else(|| wire::failure("invalid Radius model catalog"))?
        .iter()
        .map(|model| {
            if model["id"].as_str().is_none_or(str::is_empty)
                || model["name"].as_str().is_none()
                || model["reasoning"].as_bool().is_none()
                || !model["input"].is_array()
                || !model["cost"].is_object()
                || model["contextWindow"].as_u64().is_none()
                || model["maxTokens"].as_u64().is_none()
            {
                return Err(wire::failure("invalid Radius model entry"));
            }
            let mut model = model.clone();
            model["api"] = json!("pi-messages");
            model["baseUrl"] = json!(base);
            Ok(model)
        })
        .collect()
}

pub(crate) fn copilot_models(body: &Value) -> Result<Vec<Value>, Fault> {
    let models = body["data"]
        .as_array()
        .ok_or_else(|| wire::failure("invalid Copilot catalog"))?;
    let eligible: Vec<_> = models
        .iter()
        .filter(|m| m["id"].is_string() && m["capabilities"]["supports"]["tool_calls"] != false)
        .collect();
    let picker: Vec<_> = eligible
        .iter()
        .filter(|m| m["model_picker_enabled"] == true && m["policy"]["state"] != "disabled")
        .map(|m| json!({ "id": m["id"] }))
        .collect();
    if !picker.is_empty() {
        return Ok(picker);
    }
    Ok(eligible
        .iter()
        .filter(|m| m["policy"]["state"] == "enabled")
        .map(|m| json!({ "id": m["id"] }))
        .collect())
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    #[test]
    fn radius_empty_catalog_is_valid_and_malformed_entries_do_not_replace_cache() {
        assert!(
            radius_models(&json!({ "baseUrl": "https://gateway.test/v1", "models": [] }))
                .unwrap()
                .is_empty()
        );
        assert!(
            radius_models(&json!({
                "baseUrl": "https://gateway.test/v1",
                "models": [{ "id": "broken" }],
            }))
            .is_err()
        );
        let models = radius_models(&json!({
            "baseUrl": "https://gateway.test/v1",
            "models": [{
                "id": "m",
                "name": "M",
                "reasoning": true,
                "input": ["text", "image"],
                "cost": {},
                "contextWindow": 8192,
                "maxTokens": 1024,
            }],
        }))
        .unwrap();
        assert_eq!(models[0]["api"], "pi-messages");
        assert_eq!(models[0]["baseUrl"], "https://gateway.test/v1");
    }
    #[test]
    fn copilot_picker_and_policy_fallback_exclude_disabled_and_non_tools() {
        let body = json!({
            "data": [
                { "id": "a", "model_picker_enabled": true },
                { "id": "b", "model_picker_enabled": true, "policy": { "state": "disabled" } },
                {
                    "id": "c",
                    "model_picker_enabled": true,
                    "capabilities": { "supports": { "tool_calls": false } },
                },
                { "id": "d", "policy": { "state": "enabled" } }
            ],
        });
        assert_eq!(copilot_models(&body).unwrap(), vec![json!({ "id": "a" })]);
        assert_eq!(
            copilot_models(&json!({ "data": [{ "id": "d", "policy": { "state": "enabled" } }] }))
                .unwrap(),
            vec![json!({ "id": "d" })]
        );
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use eden_protocol::models::CredentialReply;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    async fn server(events: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let n = socket.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..split]).to_ascii_lowercase();
                    let size: usize = header
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if bytes.len() >= split + 4 + size {
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: \
                 {}\r\nConnection: close\r\n\r\n{events}",
                events.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(bytes).unwrap()
        });
        (base, task)
    }
    fn credential() -> CredentialReply {
        CredentialReply {
            api_key: Some("token".into()),
            headers: Default::default(),
            source: "test".into(),
            base_url: None,
            available_model_ids: None,
            catalog_scope: None,
        }
    }
    fn input() -> ModelInput {
        serde_json::from_value(json!({
            "items": [{
                "type": "message",
                "role": "user",
                "content": [{ "type": "text", "text": "hello" }],
            }],
            "tools": [],
        }))
        .unwrap()
    }
    #[tokio::test]
    async fn codex_distinct_endpoint_body_and_private_account_header_reach_server() {
        let (base, server) = server(include_str!("fixtures/subscription-1.txt")).await;
        let mut target = projection::test_target("openai-codex-responses");
        target.base_url = format!("{base}/backend-api");
        target.provider = "openai-codex".into();
        target.compat = json!({ "transport": "sse" });
        let mut credential = credential();
        credential
            .headers
            .insert("chatgpt-account-id".into(), "account".into());
        let reply = crate::targeted::request(&target, &credential, &input(), |_, _| Ok(()))
            .await
            .unwrap();
        assert!(
            matches!(&reply.items[0], Item::Message { content, .. } if content == &vec![Block::Text { text:"EDEN_G3_OK".into() }])
        );
        assert_eq!(reply.usage["raw"]["input_tokens"], 2);
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /backend-api/codex/responses "));
        assert!(request.contains("chatgpt-account-id: account"));
        assert!(request.contains("responses=experimental"));
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["store"], false);
        assert!(body.get("max_output_tokens").is_none());
    }
    #[tokio::test]
    async fn radius_requires_terminal_and_uses_messages_not_anthropic_endpoint() {
        let (base, server) = server(include_str!("fixtures/subscription-2.txt")).await;
        let mut target = projection::test_target("pi-messages");
        target.base_url = format!("{base}/v1");
        target.provider = "radius".into();
        let error = crate::targeted::request(&target, &credential(), &input(), |_, _| Ok(()))
            .await
            .unwrap_err();
        assert_eq!(error.code, "RetryableProviderFailure");
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /v1/messages "));
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["context"]["messages"][0]["role"], "user");
    }
    #[tokio::test]
    async fn copilot_anthropic_uses_bearer_and_agent_header() {
        for provider in ["github-copilot", "anthropic", "kimi-coding"] {
            let (base, server) = server(include_str!("fixtures/subscription-3.txt")).await;
            let mut target = projection::test_target("anthropic-messages");
            target.base_url = base;
            target.provider = provider.into();
            let mut credential = credential();
            if provider != "github-copilot" {
                credential
                    .headers
                    .insert("aUtHoRiZaTiOn".into(), "Bearer token".into());
            }
            crate::targeted::request(&target, &credential, &input(), |_, _| Ok(()))
                .await
                .unwrap();
            let request = server.await.unwrap().to_ascii_lowercase();
            assert!(request.contains("authorization: bearer token"));
            assert!(!request.contains("x-api-key:"));
            if provider == "github-copilot" {
                assert!(request.contains("x-initiator: user"));
            }
        }
    }
}

/// Login owns model policy activation; catalog reads never silently enable policies.
pub(crate) async fn copilot_account_models(
    access: &str,
    base_url: &str,
    headers: &BTreeMap<String, String>,
) -> Result<Vec<String>, Fault> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|_| wire::failure("Copilot catalog client initialization failed"))?;
    let base =
        reqwest::Url::parse(base_url).map_err(|_| wire::failure("invalid Copilot endpoint"))?;
    if !matches!(base.scheme(), "http" | "https")
        || !base.username().is_empty()
        || base.password().is_some()
    {
        return Err(wire::failure("invalid Copilot endpoint"));
    }
    let configure = |mut request: reqwest::RequestBuilder| {
        request = request
            .bearer_auth(access)
            .header("User-Agent", "eden-agent");
        for (name, value) in headers {
            request = request.header(name, value);
        }
        request
    };
    let response = configure(client.get(format!("{}/models", base_url.trim_end_matches('/'))))
        .send()
        .await
        .map_err(|_| wire::failure("Copilot model discovery failed"))?;
    if !response.status().is_success() {
        return Err(wire::failure("Copilot model discovery rejected"));
    }
    let body = response
        .json::<Value>()
        .await
        .map_err(|_| wire::failure("invalid Copilot catalog"))?;
    let mut available: Vec<String> = copilot_models(&body)?
        .iter()
        .filter_map(|m| m["id"].as_str().map(str::to_owned))
        .collect();
    let known: Value = serde_json::from_str(include_str!("../data/pi-models.json"))
        .map_err(|_| wire::failure("invalid bundled Copilot catalog"))?;
    let models = body["data"]
        .as_array()
        .ok_or_else(|| wire::failure("invalid Copilot catalog"))?;
    let picker = models.iter().any(|m| {
        m["model_picker_enabled"] == true
            && m["policy"]["state"] != "disabled"
            && m["capabilities"]["supports"]["tool_calls"] != false
    });
    for model in models {
        let Some(id) = model["id"].as_str() else {
            continue;
        };
        if model["policy"]["state"] != "unconfigured"
            || model["capabilities"]["supports"]["tool_calls"] == false
            || (picker && model["model_picker_enabled"] != true)
            || !known["github-copilot"]
                .as_object()
                .is_some_and(|apis| apis.values().any(|models| models.get(id).is_some()))
        {
            continue;
        }
        let mut url = base.clone();
        url.path_segments_mut()
            .map_err(|_| wire::failure("invalid Copilot endpoint"))?
            .pop_if_empty()
            .push("models")
            .push(id)
            .push("policy");
        let response = configure(client.post(url))
            .header("openai-intent", "chat-policy")
            .header("x-interaction-type", "chat-policy")
            .json(&json!({ "state": "enabled" }))
            .send()
            .await
            .map_err(|_| wire::failure("Copilot model policy request failed"))?;
        if response.status().is_success() {
            available.push(id.into());
        } else {
            available.retain(|candidate| candidate != id);
        }
    }
    available.sort();
    available.dedup();
    Ok(available)
}

pub(crate) fn catalog_scope(credential: &eden_protocol::models::CredentialReply) -> String {
    use sha2::{Digest, Sha256};
    credential.catalog_scope.clone().unwrap_or_else(|| {
        credential
            .api_key
            .as_ref()
            .map(|key| format!("{:x}", Sha256::digest(key.as_bytes())))
            .unwrap_or_else(|| "public".into())
    })
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    #[tokio::test]
    async fn login_activates_only_known_eligible_models_with_explicit_client_headers() {
        let bundled: Value = serde_json::from_str(include_str!("../data/pi-models.json")).unwrap();
        let id = bundled["github-copilot"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let expected = id.clone();
        let server = tokio::spawn(async move {
            for policy in [false, true] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = [0; 8192];
                let n = socket.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..n]).to_ascii_lowercase();
                assert!(request.contains("copilot-integration-id: admitted-eden-client"));
                assert!(request.contains("user-agent: eden-agent"));
                let body =
                    if policy {
                        assert!(request.starts_with(&format!(
                            "post /models/{}/policy ",
                            expected.to_lowercase()
                        )));
                        "{}".into()
                    } else {
                        json!({
                            "data": [
                                {
                                    "id": expected,
                                    "model_picker_enabled": true,
                                    "policy": { "state": "unconfigured" },
                                },
                                {
                                    "id": "unknown-model",
                                    "model_picker_enabled": true,
                                    "policy": { "state": "unconfigured" },
                                }
                            ],
                        })
                        .to_string()
                    };
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: \
                             {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        let ids = copilot_account_models(
            "token",
            &base,
            &BTreeMap::from([(
                "Copilot-Integration-Id".into(),
                "admitted-eden-client".into(),
            )]),
        )
        .await
        .unwrap();
        assert!(ids.contains(&id));
        server.await.unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codex_moves_system_to_instructions_and_omits_unsupported_limit() {
        let mut body = json!({
            "input": [
                { "role": "system", "content": [{ "text": "rules" }] },
                { "role": "user", "content": [] }
            ],
            "max_output_tokens": 10,
        });
        codex_body(&mut body);
        assert_eq!(body["instructions"], "rules");
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["store"], false);
    }
    #[test]
    fn copilot_initiator_tracks_last_item_and_vision() {
        let input: ModelInput = serde_json::from_value(json!({
            "items": [{
                "type": "message",
                "role": "user",
                "content": [{ "type": "image", "media_type": "image/png", "data": "abc" }],
            }],
            "tools": [],
        }))
        .unwrap();
        let mut target = projection::test_target("anthropic-messages");
        target.provider = "github-copilot".into();
        let headers = headers(&target, &input);
        assert_eq!(headers["X-Initiator"], "user");
        assert_eq!(headers["Copilot-Vision-Request"], "true");
    }
}
