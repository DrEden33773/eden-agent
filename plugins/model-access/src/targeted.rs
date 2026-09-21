//! Frozen catalog routes share transport and cancellation with the legacy provider.
use super::{
    Profile, RequestOptions, anthropic, chat, gemini, mistral, projection, routes, usage, wire,
};
use eden_protocol::{
    Fault,
    coding::{Item, ModelInput, ModelReply},
    models::{CredentialReply, ModelTarget},
};
use futures_util::StreamExt;
use serde_json::Value;

pub(crate) async fn request(
    target: &ModelTarget,
    credential: &CredentialReply,
    input: &ModelInput,
    mut emit: impl FnMut(&str, Value) -> Result<(), Fault>,
) -> Result<ModelReply, Fault> {
    if target.api == "bedrock-converse-stream" {
        return crate::bedrock::request(target, credential, input, emit).await;
    }
    let options = RequestOptions {
        profile: if target.provider == "deepseek" {
            Profile::Deepseek
        } else {
            Profile::Openai
        },
        max_output_tokens: Some(target.limits.max_output_tokens).filter(|n| *n > 0),
        reasoning_effort: projection::effort(target)
            .filter(|_| target.capabilities.reasoning)
            .as_ref()
            .map(|effort| {
                if effort == "off" {
                    "none".into()
                } else {
                    effort.clone()
                }
            }),
    };
    let mut projected = input.clone();
    projected.items = projection::items(input, target)?;
    let (suffix, mut body) = match target.api.as_str() {
        "openai-completions" => ("chat/completions", chat::project(input, target)?),
        "mistral-conversations" => ("chat/completions", mistral::project(input, target)?),
        "google-generative-ai" | "google-vertex" => ("", gemini::project(input, target)?),
        "anthropic-messages" => ("v1/messages", anthropic::project(input, target)?),
        "pi-messages" => ("messages", crate::pi_messages::project(input, target)?),
        "openai-responses" | "azure-openai-responses" | "openai-codex-responses" => {
            for item in &mut projected.items {
                if let Item::ProviderState { provider, value } = item {
                    *provider = options.profile.state().into();
                    *value = projection::raw_state(value).clone();
                }
            }
            (
                "responses",
                wire::project(
                    &projected,
                    target.compat["deployment"]
                        .as_str()
                        .filter(|_| target.api == "azure-openai-responses")
                        .unwrap_or(&target.model),
                    &options,
                )?,
            )
        }
        _ => return Err(wire::failure("selected model protocol is not implemented")),
    };
    if target.api == "openai-codex-responses" {
        crate::subscription::codex_body(&mut body);
    }
    let url = routes::endpoint(target, credential, suffix)?;
    if target.api == "openai-codex-responses"
        && let Some(reply) = crate::codex_socket::request(
            target, credential, input, &options, &body, &url, &mut emit,
        )
        .await?
    {
        return Ok(reply);
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|_| wire::failure("HTTP client initialization failed"))?;
    let mut request = client
        .post(url)
        .header("Accept", "text/event-stream")
        .json(&body);
    for (name, value) in crate::subscription::headers(target, input) {
        request = request.header(name, value);
    }
    for (name, value) in &target.headers {
        request = request.header(name, value);
    }
    if target.provider == "cloudflare-ai-gateway" {
        if let Some(key) = &credential.api_key {
            request = request.header("cf-aig-authorization", format!("Bearer {key}"));
        }
        if target.api == "anthropic-messages" {
            request = request.header("anthropic-version", "2023-06-01");
        }
    } else if target.api == "anthropic-messages" {
        request = request.header("anthropic-version", "2023-06-01");
        if let Some(key) = &credential.api_key
            && !credential
                .headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("authorization"))
        {
            request = if target.provider == "github-copilot" {
                request.bearer_auth(key)
            } else {
                request.header("x-api-key", key)
            };
        }
    } else if matches!(
        target.api.as_str(),
        "google-generative-ai" | "google-vertex"
    ) {
        if let Some(key) = &credential.api_key {
            request = request.header("x-goog-api-key", key);
        }
    } else if target.api == "azure-openai-responses" {
        if let Some(key) = &credential.api_key {
            request = request.header("api-key", key);
        }
    } else if let Some(key) = &credential.api_key {
        request = request.bearer_auth(key);
    }
    for (name, value) in &credential.headers {
        if name.to_ascii_lowercase().starts_with("x-eden-private-") {
            return Err(wire::failure(
                "private cloud credential cannot be used with this protocol",
            ));
        }
        request = request.header(name, value);
    }
    let response = request.send().await.map_err(super::transport_failure)?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let retry_after_ms = retry_delay(response.headers());
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(Ok(chunk)) = stream.next().await {
            if bytes.len() + chunk.len() > 65536 {
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        let mut error = wire::provider_fault(
            Some(status),
            &serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        );
        error.retry_after_ms = retry_after_ms;
        return Err(error);
    }
    let mut stream = response.bytes_stream();
    let mut sse = wire::Sse::default();
    let mut responses = wire::ResponseOutput::default();
    let mut pi = crate::pi_messages::Decoder::new(target.clone());
    let mut chat = chat::Decoder::new(target.clone());
    let mut anthropic = anthropic::Decoder::new(target.clone());
    let mut gemini = gemini::Decoder::new(target.clone());
    let mut mistral = mistral::Decoder::new(target.clone());
    while let Some(chunk) = stream.next().await {
        for event in sse.push(&chunk.map_err(super::transport_failure)?)? {
            if matches!(
                target.api.as_str(),
                "openai-responses" | "azure-openai-responses" | "openai-codex-responses"
            ) {
                responses.observe(&event)?;
            }
            match target.api.as_str() {
                "pi-messages" => {
                    let delta = match event["type"].as_str() {
                        Some("text_delta") => Some("model_text_delta"),
                        Some("thinking_delta") => Some("model_reasoning_delta"),
                        Some("toolcall_delta") => Some("model_tool_delta"),
                        _ => None,
                    };
                    if let Some(kind) = delta {
                        emit(kind, event.clone())?;
                    }
                    if let Some(reply) = pi.consume(event)? {
                        return Ok(reply);
                    }
                }
                "google-generative-ai" | "google-vertex" => {
                    let reply = gemini.consume(event)?;
                    for (kind, payload) in gemini.take_deltas() {
                        emit(kind, payload)?;
                    }
                    if let Some(reply) = reply {
                        return Ok(reply);
                    }
                }
                "mistral-conversations" => {
                    let reply = mistral.consume(event)?;
                    for (kind, payload) in mistral.take_deltas() {
                        emit(kind, payload)?;
                    }
                    if let Some(reply) = reply {
                        return Ok(reply);
                    }
                }
                "openai-completions" => {
                    let reply = chat.consume(event)?;
                    for (kind, payload) in chat.take_deltas() {
                        emit(kind, payload)?;
                    }
                    if let Some(reply) = reply {
                        return Ok(reply);
                    }
                }
                "anthropic-messages" => {
                    let reply = anthropic.consume(event)?;
                    for (kind, payload) in anthropic.take_deltas() {
                        emit(kind, payload)?;
                    }
                    if let Some(reply) = reply {
                        return Ok(reply);
                    }
                }
                _ => match event["type"].as_str() {
                    Some("response.completed" | "response.done") => {
                        let mut reply = responses.complete(&event["response"], options.profile)?;
                        for item in &mut reply.items {
                            if let Item::ProviderState { value, .. } = item {
                                *item = projection::state(target, value.clone());
                            }
                        }
                        let mut accounting_target = target.clone();
                        if accounting_target.api == "openai-codex-responses" {
                            accounting_target.api = "openai-responses".into();
                        }
                        reply.usage =
                            usage::normalize(&reply.usage, &accounting_target, Some("stop"));
                        return Ok(reply);
                    }
                    Some("response.incomplete") => {
                        return Err(wire::incomplete(&event["response"], input, &options));
                    }
                    Some("response.failed" | "error") => {
                        return Err(wire::provider_fault(None, &event));
                    }
                    Some("response.output_text.delta" | "response.refusal.delta") => {
                        emit("model_text_delta", event)?
                    }
                    Some(
                        "response.reasoning_text.delta" | "response.reasoning_summary_text.delta",
                    ) => emit("model_reasoning_delta", event)?,
                    Some("response.function_call_arguments.delta") => {
                        emit("model_tool_delta", event)?
                    }
                    _ => {}
                },
            }
        }
    }
    Err(Fault::new(
        "RetryableProviderFailure",
        "model-access",
        "stream ended before its completion marker",
    ))
}

pub(crate) fn retry_delay(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    retry_delay_at(headers, std::time::SystemTime::now())
}
fn retry_delay_at(headers: &reqwest::header::HeaderMap, now: std::time::SystemTime) -> Option<u64> {
    let value = headers.get("retry-after")?.to_str().ok()?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds.saturating_mul(1000));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    Some(
        date.duration_since(now)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_after_accepts_seconds_and_http_dates_without_exposing_header() {
        let mut headers = reqwest::header::HeaderMap::new();
        let now = httpdate::parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT").unwrap();
        headers.insert("retry-after", "12".parse().unwrap());
        assert_eq!(retry_delay_at(&headers, now), Some(12000));
        headers.insert(
            "retry-after",
            "Wed, 21 Oct 2015 07:28:05 GMT".parse().unwrap(),
        );
        assert_eq!(retry_delay_at(&headers, now), Some(5000));
        headers.insert("retry-after", "secret-canary".parse().unwrap());
        assert_eq!(retry_delay_at(&headers, now), None);
    }
}
