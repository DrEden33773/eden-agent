//! OpenAI and DeepSeek Responses access through the ordinary coding-provider service.
//! HTTP clients, request bodies and streaming decoders live in the call future;
//! dropping that future during scope cancellation closes its response stream.
use eden_plugin_sdk::{Package, protocol::Descriptor};
use eden_protocol::{
    Fault,
    coding::{MODEL_INFO, ModelInput, ModelLimits, ModelReply, PROVIDER},
};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use wire::{Sse, failure};
mod anthropic;
mod catalog;
mod chat;
mod credentials;
mod projection;
mod targeted;
mod usage;
mod wire;

static NEXT_ATTEMPT: AtomicU64 = AtomicU64::new(1);

const DEEPSEEK_CONTEXT_WINDOW: u64 = 1_048_576;
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
struct Config {
    model: Option<String>,
    endpoint: Option<String>,
    api_key_env: Option<String>,
    profile: Option<Profile>,
    max_output_tokens: Option<u32>,
    reasoning_effort: Option<String>,
    context_window: Option<u64>,
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
        Ok(Settings {
            model,
            endpoint,
            key,
            options: self.options_with(&env)?,
        })
    }
    fn options_with(&self, env: impl Fn(&str) -> Option<String>) -> Result<RequestOptions, Fault> {
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
        Ok(RequestOptions {
            profile,
            max_output_tokens,
            reasoning_effort,
        })
    }
    fn limits_with(&self, env: impl Fn(&str) -> Option<String>) -> Result<ModelLimits, Fault> {
        let options = self.options_with(env)?;
        Ok(ModelLimits {
            context_window: self.context_window.unwrap_or_else(|| {
                if options.profile == Profile::Deepseek {
                    DEEPSEEK_CONTEXT_WINDOW
                } else {
                    0
                }
            }),
            max_output_tokens: options.max_output_tokens.unwrap_or(0),
        })
    }
}
fn descriptor() -> Descriptor {
    Descriptor {
        package: "model-access".into(),
        version: "0.1.0".into(),
        provides: vec![
            eden_protocol::models::MODEL_CATALOG.into(),
            eden_protocol::models::CREDENTIAL_SOURCE.into(),
            eden_protocol::models::AUTH.into(),
            MODEL_INFO.into(),
            PROVIDER.into(),
        ],
    }
}
fn create(config: Value) -> Result<Package, Fault> {
    let package = credentials::register(
        catalog::register(Package::new("model-access"), &config)?,
        &config,
    )?;
    let config: Config = serde_json::from_value(if config.is_null() {
        serde_json::json!({})
    } else {
        config
    })
    .map_err(|_| failure("invalid model-access configuration"))?;
    let config = Arc::new(config);
    let info_config = config.clone();
    Ok(package
        .service(MODEL_INFO, move |_: (), _| {
            let config = info_config.clone();
            async move { config.limits_with(|key| std::env::var(key).ok()) }
        })
        .service(PROVIDER, move |input: ModelInput, cx| {
            let config = config.clone();
            async move {
                let cancel = cx.scope.cancellation();
                let attempt_id = format!(
                    "{}:{}",
                    cx.run_id(),
                    NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed)
                );
                cx.emit(
                    "model_attempt_started",
                    serde_json::json!({ "attempt_id": attempt_id }),
                )?;
                let result = tokio::select! {
                    biased;
                    _ = cancel.cancelled() =>
                        Err(Fault::new("Cancelled", "model-access", "request cancelled",)),
                    reply = async {
                            let emit = |kind: &str, mut payload: Value| {
                                payload["attempt_id"] = serde_json::json!(attempt_id);
                                cx.emit(kind, payload)
                            };
                            if let Some(target) = &input.target {
                                let credentials: eden_protocol::models::CredentialReply = cx
                                    .call(
                                        eden_protocol::models::CREDENTIAL_SOURCE,
                                        &eden_protocol::models::CredentialRequest {
                                            provider: target.provider.clone(),
                                            explicit: None,
                                            purpose: "inference".into(),
                                        },
                                    )
                                    .await?;
                                targeted::request(target, &credentials, &input, emit).await
                            } else {
                                let settings = config.settings()?;
                                request(
                                    &settings.endpoint,
                                    &settings.model,
                                    &settings.key,
                                    &input,
                                    &settings.options,
                                    emit,
                                )
                                .await
                            }
                        } => reply,
                };
                let status = match &result {
                    Ok(_) => "completed",
                    Err(error) if error.code == "Cancelled" => "cancelled",
                    Err(_) => "failed",
                };
                cx.emit(
                    "model_attempt_finished",
                    serde_json::json!({
                        "attempt_id": attempt_id,
                        "status": status,
                        "error": result.as_ref().err(),
                        "text": result.as_ref().ok().map(|reply| reply
                            .items
                            .iter()
                            .filter_map(|item| {
                                if let eden_protocol::coding::Item::Message { role, content } = item
                                {
                                    (role == "assistant").then(|| {
                                        content
                                            .iter()
                                            .filter_map(|block| {
                                                if let eden_protocol::coding::Block::Text { text } =
                                                    block
                                                {
                                                    Some(text.as_str())
                                                } else {
                                                    None
                                                }
                                            })
                                            .collect::<String>()
                                    })
                                } else {
                                    None
                                }
                            })
                            .collect::<String>()),
                    }),
                )?;
                result
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
        .map_err(transport_failure)?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let retry_after_ms = targeted::retry_delay(response.headers());
        // Parse only a bounded error envelope and never expose its contents.
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else { break };
            if bytes.len() + chunk.len() > 64 * 1024 {
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let mut error = wire::provider_fault(Some(status), &body);
        error.retry_after_ms = retry_after_ms;
        return Err(error);
    }
    let mut stream = response.bytes_stream();
    let mut sse = Sse::default();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport_failure)?;
        for event in sse.push(&chunk)? {
            match event.get("type").and_then(Value::as_str) {
                Some("response.completed") => {
                    return wire::completed(&event["response"], options.profile);
                }
                Some("response.incomplete") => {
                    return Err(wire::incomplete(&event["response"], input, options));
                }
                Some("response.failed") => {
                    return Err(wire::provider_fault(None, &event["response"]));
                }
                Some("error") => return Err(wire::provider_fault(None, &event)),
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
    Err(Fault::new(
        "RetryableProviderFailure",
        "model-access",
        "Responses stream ended before response.completed",
    ))
}
fn transport_failure(error: reqwest::Error) -> Fault {
    if !error.is_builder()
        && (error.is_timeout() || error.is_connect() || error.is_body() || error.is_request())
    {
        Fault::new(
            "RetryableProviderFailure",
            "model-access",
            "Responses transport interrupted",
        )
    } else {
        failure("Responses request transport failed")
    }
}
#[cfg(test)]
mod tests;
