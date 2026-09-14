//! OpenAI Responses access through the ordinary coding-provider service.
//! HTTP clients, request bodies and streaming decoders live in the call future;
//! dropping that future during scope cancellation closes its response stream.
use eden_plugin_sdk::{Package, protocol::Descriptor};
use eden_protocol::{
    Fault,
    coding::{ModelInput, ModelReply, PROVIDER},
};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use wire::{Sse, failure};
mod wire;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    model: Option<String>,
    endpoint: Option<String>,
    api_key_env: Option<String>,
}
impl Config {
    fn settings(&self) -> Result<(String, String, String), Fault> {
        let model = self
            .model
            .clone()
            .or_else(|| std::env::var("OPENAI_MODEL").ok())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| failure("set model or OPENAI_MODEL explicitly"))?;
        let endpoint = self.endpoint.clone().unwrap_or_else(|| {
            format!(
                "{}/responses",
                std::env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".into())
                    .trim_end_matches('/')
            )
        });
        let key = std::env::var(self.api_key_env.as_deref().unwrap_or("OPENAI_API_KEY"))
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                failure("configured API key environment variable is missing or empty")
            })?;
        Ok((model, endpoint, key))
    }
}
fn descriptor() -> Descriptor {
    Descriptor {
        package: "model-access".into(),
        version: "0.1.0".into(),
        provides: vec![PROVIDER.into()],
    }
}
fn create(config: Value) -> Result<Package, Fault> {
    let config: Config = serde_json::from_value(if config.is_null() {
        serde_json::json!({})
    } else {
        config
    })
    .map_err(|_| failure("invalid model-access configuration"))?;
    let config = Arc::new(config);
    Ok(Package::new("model-access").service(PROVIDER,move |input: ModelInput,cx| {
        let config = config.clone();
        async move {
            let (model,endpoint,key) = config.settings()?;
            let cancel = cx.scope.cancellation();
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(Fault::new("Cancelled","model-access","request cancelled")),
                reply = request(&endpoint,&model,&key,&input,|kind,payload|cx.emit(kind,payload)) => reply,
            }
        }
    }))
}
eden_plugin_sdk::export_plugin!(descriptor, create);

async fn request(
    endpoint: &str,
    model: &str,
    key: &str,
    input: &ModelInput,
    mut emit: impl FnMut(&str, Value) -> Result<(), Fault>,
) -> Result<ModelReply, Fault> {
    let body = wire::project(input, model)?;
    let url =
        reqwest::Url::parse(endpoint).map_err(|_| failure("invalid Responses endpoint URL"))?;
    if !["http", "https"].contains(&url.scheme())
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(failure(
            "Responses endpoint must use HTTP(S) without URL credentials",
        ));
    }
    // A per-call client prevents pooled connections from outliving the managed request.
    // Disable redirects so a configured endpoint cannot forward credentials elsewhere.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|_| failure("HTTP client initialization failed"))?;
    let response = client
        .post(url)
        .bearer_auth(key)
        .header("Accept", "text/event-stream")
        .json(&body)
        .send()
        .await
        .map_err(|_| failure("Responses request transport failed"))?;
    if !response.status().is_success() {
        return Err(failure(format!(
            "Responses HTTP status {}",
            response.status().as_u16()
        )));
    }
    let mut stream = response.bytes_stream();
    let mut sse = Sse::default();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| failure("Responses stream transport failed"))?;
        for event in sse.push(&chunk)? {
            match event.get("type").and_then(Value::as_str) {
                Some("response.completed") => return wire::completed(&event["response"]),
                Some("response.failed" | "response.incomplete" | "error") => {
                    return Err(failure(
                        "Responses stream reported failure or incomplete output",
                    ));
                }
                Some("response.output_text.delta" | "response.refusal.delta") => {
                    emit("model_text_delta", event)?
                }
                Some("response.function_call_arguments.delta") => emit("model_tool_delta", event)?,
                Some(_) => {}
                None => return Err(failure("stream event is missing its type")),
            }
        }
    }
    Err(failure("Responses stream ended before response.completed"))
}
#[cfg(test)]
mod tests;
