//! OpenAI and DeepSeek Responses access through the ordinary coding-provider service.
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

const DEEPSEEK_MAX_OUTPUT_TOKENS: u32 = 393_216;

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Profile {
    #[default]
    Openai,
    Deepseek,
}
impl Profile {
    fn state(self) -> &'static str {
        match self {
            Self::Openai => wire::STATE,
            Self::Deepseek => "deepseek-responses",
        }
    }
}
#[derive(Default)]
struct RequestOptions {
    profile: Profile,
    max_output_tokens: Option<u32>,
    reasoning_effort: Option<String>,
}
struct Settings {
    model: String,
    endpoint: String,
    key: String,
    options: RequestOptions,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    model: Option<String>,
    endpoint: Option<String>,
    api_key_env: Option<String>,
    profile: Option<Profile>,
    max_output_tokens: Option<u32>,
    reasoning_effort: Option<String>,
}
impl Config {
    fn settings(&self) -> Result<Settings, Fault> {
        self.settings_with(|key| std::env::var(key).ok())
    }
    fn settings_with(&self, env: impl Fn(&str) -> Option<String>) -> Result<Settings, Fault> {
        let model = self
            .model
            .clone()
            .or_else(|| env("OPENAI_MODEL"))
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| failure("set model or OPENAI_MODEL explicitly"))?;
        let endpoint = self.endpoint.clone().unwrap_or_else(|| {
            format!(
                "{}/responses",
                env("OPENAI_BASE_URL")
                    .unwrap_or_else(|| "https://api.openai.com/v1".into())
                    .trim_end_matches('/')
            )
        });
        let key_env = self
            .api_key_env
            .clone()
            .or_else(|| env("EDEN_API_KEY_ENV"))
            .unwrap_or_else(|| "OPENAI_API_KEY".into());
        let key = env(&key_env)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                failure("configured API key environment variable is missing or empty")
            })?;
        let profile = match self.profile {
            Some(profile) => profile,
            None => match env("EDEN_RESPONSES_PROFILE").as_deref() {
                None | Some("openai") => Profile::Openai,
                Some("deepseek") => Profile::Deepseek,
                _ => return Err(failure("Responses profile must be openai or deepseek")),
            },
        };
        let max_output_tokens = match self.max_output_tokens {
            Some(tokens) => Some(tokens),
            None => env("OPENAI_MAX_OUTPUT_TOKENS")
                .map(|tokens| {
                    tokens
                        .parse::<u32>()
                        .map_err(|_| failure("max_output_tokens must be a positive integer"))
                })
                .transpose()?,
        }
        .or_else(|| (profile == Profile::Deepseek).then_some(DEEPSEEK_MAX_OUTPUT_TOKENS));
        if max_output_tokens == Some(0) {
            return Err(failure("max_output_tokens must be a positive integer"));
        }
        let reasoning_effort = self
            .reasoning_effort
            .clone()
            .or_else(|| env("OPENAI_REASONING_EFFORT"))
            .or_else(|| (profile == Profile::Deepseek).then(|| "high".into()));
        if reasoning_effort
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(failure("reasoning_effort must not be empty"));
        }
        Ok(Settings {
            model,
            endpoint,
            key,
            options: RequestOptions {
                profile,
                max_output_tokens,
                reasoning_effort,
            },
        })
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
            let settings = config.settings()?;
            let cancel = cx.scope.cancellation();
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(Fault::new("Cancelled","model-access","request cancelled")),
                reply = request(&settings.endpoint,&settings.model,&settings.key,&input,&settings.options,|kind,payload|cx.emit(kind,payload)) => reply,
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
    options: &RequestOptions,
    mut emit: impl FnMut(&str, Value) -> Result<(), Fault>,
) -> Result<ModelReply, Fault> {
    let body = wire::project(input, model, options)?;
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
                Some("response.completed") => {
                    return wire::completed(&event["response"], options.profile);
                }
                Some("response.failed" | "response.incomplete" | "error") => {
                    return Err(failure(
                        "Responses stream reported failure or incomplete output",
                    ));
                }
                Some("response.output_text.delta" | "response.refusal.delta") => {
                    emit("model_text_delta", event)?
                }
                Some("response.reasoning_text.delta" | "response.reasoning_summary_text.delta") => {
                    emit("model_reasoning_delta", event)?
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
