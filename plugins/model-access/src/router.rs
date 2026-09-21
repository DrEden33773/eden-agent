//! Router operations distinguish accepted requests from observed remote completion.
use eden_plugin_sdk::{CallContext, Package};
use eden_protocol::{Fault, coding::ModelLimits, models::*};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use std::time::Duration;

#[derive(Clone, Deserialize)]
#[serde(default)]
struct Config {
    url: Option<String>,
    search_url: String,
    offline: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            url: None,
            search_url: "https://huggingface.co/api/models".into(),
            offline: false,
        }
    }
}
fn fault(message: &str) -> Fault {
    Fault::new("RouterFailure", "model-manager", message)
}
fn unknown() -> Fault {
    Fault::new(
        "RemoteStateUnknown",
        "model-manager",
        "router state could not be confirmed; reconnect to inspect, do not replay the operation",
    )
}
fn validate_url(value: &str) -> Result<(), Fault> {
    let url = reqwest::Url::parse(value).map_err(|_| fault("invalid router/search URL"))?;
    if !["http", "https"].contains(&url.scheme())
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(fault(
            "router/search URL requires HTTP(S) without credentials, query or fragment",
        ));
    }
    Ok(())
}
pub(crate) fn register(package: Package, value: &Value) -> Result<Package, Fault> {
    let mut config: Config =
        serde_json::from_value(value.get("router").cloned().unwrap_or(json!({})))
            .map_err(|_| fault("invalid router configuration"))?;
    config.url = config.url.or_else(|| std::env::var("LLAMA_BASE_URL").ok());
    config.offline |= value["catalog"]["offline"].as_bool() == Some(true);
    if let Some(url) = &mut config.url {
        *url = url.trim_end_matches('/').trim_end_matches("/v1").to_owned();
        validate_url(url)?;
    }
    validate_url(&config.search_url)?;
    Ok(
        package.service(MODEL_MANAGER, move |request: ManagerRequest, cx| {
            let config = config.clone();
            async move { handle(config, request, cx).await }
        }),
    )
}

fn snapshot(data: &Value, props: &Value, base: &str) -> Result<ManagerReply, Fault> {
    let autoload = props["models_autoload"].as_bool().unwrap_or(false);
    let models = data["data"]
        .as_array()
        .ok_or_else(|| fault("invalid router model list"))?
        .iter()
        .map(|m| {
            let id = m["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| fault("router model has no identity"))?;
            let state = m["status"]["value"]
                .as_str()
                .ok_or_else(|| fault("router model has no state"))?;
            let failed = m["status"]["failed"].as_bool().unwrap_or(false);
            let source = m["source"].as_str().unwrap_or("");
            let selectable = matches!(state, "loaded" | "sleeping")
                || (autoload && state == "unloaded" && source == "preset" && !failed);
            let context_window = m["meta"]["n_ctx"]
                .as_u64()
                .filter(|n| *n > 0)
                .or_else(|| m["meta"]["n_ctx_train"].as_u64().filter(|n| *n > 0))
                .unwrap_or(128000);
            Ok(ManagedModel {
                id: id.into(),
                state: state.into(),
                source: source.into(),
                failed,
                selectable,
                progress: progress(&m["status"]["progress"]),
                target: ModelTarget {
                    provider: "llama.cpp".into(),
                    model: id.into(),
                    api: "openai-completions".into(),
                    base_url: format!("{base}/v1"),
                    limits: ModelLimits {
                        context_window,
                        max_output_tokens: context_window.min(u32::MAX as u64) as u32,
                    },
                    capabilities: ModelCapabilities {
                        tools: true,
                        images: m["architecture"]["input_modalities"]
                            .as_array()
                            .is_some_and(|a| a.iter().any(|v| v == "image")),
                        reasoning: false,
                    },
                    source: CatalogSource {
                        kind: "router".into(),
                        location: base.into(),
                        updated_at: None,
                    },
                    compat: json!({
                        "supportsStore": false,
                        "supportsDeveloperRole": false,
                        "supportsReasoningEffort": false,
                        "supportsUsageInStreaming": true,
                        "supportsStrictMode": false,
                        "maxTokensField": "max_tokens",
                    }),
                    ..Default::default()
                },
            })
        })
        .collect::<Result<Vec<_>, Fault>>()?;
    Ok(ManagerReply {
        status: "connected".into(),
        models,
        autoload,
        max_instances: props["max_instances"].as_u64(),
        ..Default::default()
    })
}
fn progress(value: &Value) -> Option<f64> {
    if let Some(ratio) = value.as_f64() {
        return ratio.is_finite().then_some(ratio.clamp(0.0, 1.0));
    }
    if let Some(ratio) = value["value"].as_f64() {
        return ratio.is_finite().then_some(ratio.clamp(0.0, 1.0));
    }
    let entries = value.as_object()?;
    let (done, total) = entries.values().fold((0.0, 0.0), |(done, total), p| {
        (
            done + p["done"].as_f64().unwrap_or(0.0),
            total + p["total"].as_f64().unwrap_or(0.0),
        )
    });
    (total > 0.0).then_some((done / total).clamp(0.0, 1.0))
}

struct Client {
    http: reqwest::Client,
    base: String,
    credential: CredentialReply,
}
impl Client {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        if let Some(key) = &self.credential.api_key {
            request = request.bearer_auth(key);
        }
        for (name, value) in &self.credential.headers {
            request = request.header(name, value);
        }
        request
    }
    async fn json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, Fault> {
        let mut request = self.request(method, path).timeout(Duration::from_secs(30));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|_| unknown())?;
        if !response.status().is_success() {
            return Err(fault(match response.status().as_u16() {
                401 | 403 => {
                    "router denied authorization; model downloads use the server's Hugging Face \
                     credentials"
                }
                404 => "router model or endpoint not found",
                _ => "router rejected the request",
            }));
        }
        response.json().await.map_err(|_| unknown())
    }
    async fn list(&self) -> Result<ManagerReply, Fault> {
        let data = self.json(reqwest::Method::GET, "/models", None).await?;
        let props = self.json(reqwest::Method::GET, "/props", None).await?;
        snapshot(&data, &props, &self.base)
    }
    async fn stop(&self, model: &str) -> Result<ManagerReply, Fault> {
        let before = self.list().await.map_err(|_| unknown())?;
        if before
            .models
            .iter()
            .find(|m| m.id == model)
            .is_none_or(|m| m.state == "unloaded")
        {
            return Ok(before);
        }
        // Completion can race stop: a rejected unload is harmless only if a new
        // remote observation proves the target is already stopped.
        if self
            .json(
                reqwest::Method::POST,
                "/models/unload",
                Some(json!({ "model": model })),
            )
            .await
            .is_err()
        {
            let after = self.list().await.map_err(|_| unknown())?;
            if after
                .models
                .iter()
                .find(|m| m.id == model)
                .is_none_or(|m| m.state == "unloaded")
            {
                return Ok(after);
            }
            return Err(unknown());
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let reply = self.list().await.map_err(|_| unknown())?;
            if reply
                .models
                .iter()
                .find(|m| m.id == model)
                .is_none_or(|m| m.state == "unloaded")
            {
                return Ok(reply);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(unknown());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

async fn handle(
    config: Config,
    request: ManagerRequest,
    cx: CallContext,
) -> Result<ManagerReply, Fault> {
    if config.offline {
        return Ok(ManagerReply {
            status: "offline".into(),
            ..Default::default()
        });
    }
    if let ManagerRequest::Search { query } = &request {
        return search(&config, query).await;
    }
    let Some(base) = config.url else {
        return if matches!(request, ManagerRequest::List | ManagerRequest::Reconnect) {
            Ok(ManagerReply {
                status: "unconfigured".into(),
                ..Default::default()
            })
        } else {
            Err(fault("configure router.url or LLAMA_BASE_URL"))
        };
    };
    let credential = cx
        .call(
            CREDENTIAL_SOURCE,
            &CredentialRequest {
                provider: "llama.cpp".into(),
                explicit: None,
                purpose: "router".into(),
            },
        )
        .await?;
    let client = Arc::new(Client {
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(0)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| fault("router client initialization failed"))?,
        base,
        credential,
    });
    if matches!(request, ManagerRequest::List | ManagerRequest::Reconnect) {
        return client.list().await;
    }
    let (model, path, unload_others) = match &request {
        ManagerRequest::Download { model } => (model, "/models", false),
        ManagerRequest::Load {
            model,
            unload_others,
        } => (model, "/models/load", *unload_others),
        ManagerRequest::Unload { model } | ManagerRequest::Cancel { model } => {
            let mut reply = client.stop(model).await?;
            reply.status = "stopped".into();
            return Ok(reply);
        }
        _ => unreachable!(),
    };
    if model.trim().is_empty() || model.chars().any(char::is_control) {
        return Err(fault(
            "model identity is empty or contains control characters",
        ));
    }
    let before = client.list().await?;
    if let Some(existing) = before.models.iter().find(|m| m.id == *model) {
        if matches!(existing.state.as_str(), "loading" | "downloading") {
            return Err(fault(
                "model already has a remote operation; inspect or explicitly cancel it",
            ));
        }
        if path == "/models/load" && matches!(existing.state.as_str(), "loaded" | "sleeping") {
            return Ok(ManagerReply {
                status: "completed".into(),
                ..before
            });
        }
    }
    // A bounded router can evict another client's model inside load(). There is no
    // atomic no-eviction flag, so shared keep-loaded requests require unlimited capacity.
    if path == "/models/load" && !unload_others && before.max_instances != Some(0) {
        return Err(fault(
            "keep-loaded requires router --models-max 0; otherwise explicitly choose unload_others",
        ));
    }
    if unload_others {
        for other in before
            .models
            .iter()
            .filter(|m| m.id != *model && matches!(m.state.as_str(), "loaded" | "sleeping"))
        {
            client.stop(&other.id).await?;
        }
    }
    let pending = Arc::new(AtomicU8::new(0));
    let cleanup_pending = pending.clone();
    let cleanup_client = client.clone();
    let cleanup_model = model.clone();
    // Runtime cancellation drops the handler. Cleanup is awaited before terminal delivery,
    // including session shutdown; an unreachable server becomes a cleanup error, never success.
    cx.scope.cleanup(async move {
        match cleanup_pending.load(Ordering::Acquire) {
            1 => {
                let _ = cleanup_client.stop(&cleanup_model).await;
                return Err(unknown());
            }
            2 => {
                cleanup_client.stop(&cleanup_model).await?;
            }
            _ => {}
        }
        Ok(())
    })?;
    let event_request = client.request(reqwest::Method::GET, "/models/sse").send();
    // Retain the POST until it settles before cancellation cleanup sends unload.
    // Otherwise a late accepted POST could start a model after an early unload.
    let post_client = client.clone();
    let post_model = model.clone();
    let post_pending = pending.clone();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    cx.scope.spawn(async move {
        post_pending.store(1, Ordering::Release);
        let result = post_client
            .json(
                reqwest::Method::POST,
                path,
                Some(json!({ "model": post_model })),
            )
            .await;
        match &result {
            Ok(_) => {
                post_pending.store(2, Ordering::Release);
            }
            Err(error) if error.code != "RemoteStateUnknown" => {
                post_pending.store(0, Ordering::Release);
            }
            _ => {}
        }
        let _ = sender.send(result);
        Ok(())
    })?;
    // Remote reads must progress even if a quiet SSE connection has not flushed headers.
    // Observation never owns a mutation and reconnecting it never resubmits a POST.
    let event_request = tokio::time::timeout(Duration::from_secs(30), event_request);
    tokio::pin!(event_request);
    let mut receiver = receiver;
    let mut accepted = false;
    let mut sse_opened = false;
    let mut stream: futures_util::stream::BoxStream<'static, _> =
        futures_util::stream::pending().boxed();
    let mut decoder = crate::wire::Sse::default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            result = &mut receiver, if !accepted => {
                result.map_err(|_| unknown())??;
                accepted = true;
                cx.emit(
                    "model_management",
                    json!({ "model": model, "status": "accepted" }),
                )?;
            },
            response = &mut event_request, if !sse_opened => {
                sse_opened = true;
                if let Ok(Ok(response)) = response
                    && response.status().is_success()
                {
                    stream = response.bytes_stream().boxed();
                }
            },
            _ = tick.tick(), if accepted => {
                let reply = client.list().await?;
                if let Some(current) = reply.models.iter().find(|m| m.id == *model) {
                    cx.emit(
                        "model_management",
                        json!({
                            "model": model,
                            "status": current.state,
                            "progress": current.progress,
                        }),
                    )?;
                    if current.failed {
                        pending.store(0, Ordering::Release);
                        return Err(fault(
                            "remote model operation failed; inspect server diagnostics and \
                             download permissions",
                        ));
                    }
                    if (path == "/models" && current.state == "unloaded")
                        || (path == "/models/load"
                            && matches!(current.state.as_str(), "loaded" | "sleeping"))
                    {
                        pending.store(0, Ordering::Release);
                        return Ok(ManagerReply {
                            status: "completed".into(),
                            ..reply
                        });
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(unknown());
                }
            },
            chunk = stream.next() => {
                let Some(Ok(bytes)) = chunk else {
                    stream = futures_util::stream::pending().boxed();
                    continue;
                };
                for event in decoder.push(&bytes)? {
                    // Whitelist only numeric progress; server errors and URLs can contain secrets.
                    if event["model"].as_str() == Some(model) || event["id"].as_str() == Some(model)
                    {
                        if event["event"] == "download_failed" {
                            pending.store(0, Ordering::Release);
                            return Err(fault(
                                "remote download failed; check server Hugging Face permissions",
                            ));
                        }
                        if let Some(progress) = progress(&event["data"]["progress"])
                            .or_else(|| progress(&event["progress"]))
                        {
                            cx.emit(
                                "model_management",
                                json!({
                                    "model": model,
                                    "status": if path == "/models/load" {
                                            "loading"
                                        } else {
                                            "downloading"
                                        },
                                    "progress": progress,
                                }),
                            )?;
                        }
                    }
                }
            }
        }
    }
}

fn quantization(filename: &str) -> Option<String> {
    let name = filename.rsplit('/').next()?.to_ascii_uppercase();
    if name.starts_with("MMPROJ") {
        return None;
    }
    let mut stem = name.strip_suffix(".GGUF")?;
    let parts: Vec<_> = stem.rsplitn(4, '-').collect();
    if parts.len() == 4
        && parts[1] == "OF"
        && [parts[0], parts[2]]
            .iter()
            .all(|p| p.len() == 5 && p.bytes().all(|c| c.is_ascii_digit()))
    {
        stem = parts[3];
    }
    for (index, _) in stem
        .char_indices()
        .filter(|(i, _)| *i == 0 || matches!(stem.as_bytes()[i - 1], b'-' | b'_' | b'.'))
    {
        let candidate = &stem[index..];
        let base = candidate.strip_prefix("UD-").unwrap_or(candidate);
        let mut parts = base.split('_');
        let head = parts.next()?;
        let suffix: Vec<_> = parts.collect();
        let valid_suffix = suffix
            .iter()
            .all(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric()));
        let quant = head
            .strip_prefix("IQ")
            .or_else(|| head.strip_prefix('Q'))
            .is_some_and(|n| {
                n.len() == 1 && n.bytes().all(|c| c.is_ascii_digit()) && !suffix.is_empty()
            });
        let float = matches!(head, "BF16" | "F16" | "F32") && suffix.is_empty();
        let mxfp = head
            .strip_prefix("MXFP")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()));
        if valid_suffix && (quant || float || mxfp) {
            return Some(candidate.into());
        }
    }
    None
}
fn repository(value: &Value) -> Result<ModelSearchResult, Fault> {
    let mut quants = std::collections::BTreeMap::<String, ModelQuantization>::new();
    if let Some(files) = value["siblings"].as_array() {
        for file in files {
            let Some(filename) = file["rfilename"].as_str() else {
                continue;
            };
            let Some(name) = quantization(filename) else {
                continue;
            };
            let quant = quants
                .entry(name.clone())
                .or_insert_with(|| ModelQuantization {
                    name,
                    bytes: Some(0),
                    files: Vec::new(),
                });
            quant.bytes = quant
                .bytes
                .zip(file["size"].as_u64())
                .and_then(|(total, size)| total.checked_add(size));
            quant.files.push(filename.into());
        }
    }
    let mut quants: Vec<_> = quants.into_values().collect();
    quants.sort_by_key(|q| {
        (
            q.name != "Q4_K_M",
            q.bytes.unwrap_or(u64::MAX),
            q.name.clone(),
        )
    });
    Ok(ModelSearchResult {
        id: value["id"]
            .as_str()
            .ok_or_else(|| fault("repository has no identity"))?
            .into(),
        downloads: value["downloads"].as_u64(),
        gated: value["gated"] == true || matches!(value["gated"].as_str(), Some("auto" | "manual")),
        quants,
    })
}
async fn search(config: &Config, query: &str) -> Result<ManagerReply, Fault> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| fault("search client initialization failed"))?;
    let exact = query.split('/').collect::<Vec<_>>();
    let request = if exact.len() == 2
        && exact
            .iter()
            .all(|s| !s.is_empty() && *s != "." && *s != "..")
    {
        let mut url =
            reqwest::Url::parse(&config.search_url).map_err(|_| fault("invalid search URL"))?;
        url.path_segments_mut()
            .map_err(|_| fault("invalid search URL"))?
            .pop_if_empty()
            .extend(&exact);
        client.get(url).query(&[("blobs", "true")])
    } else {
        client.get(&config.search_url).query(&[
            ("search", query),
            ("filter", "gguf"),
            ("limit", "20"),
            ("sort", "downloads"),
            ("direction", "-1"),
        ])
    };
    let response = request
        .send()
        .await
        .map_err(|_| fault("Hugging Face search unavailable"))?;
    if !response.status().is_success() {
        return Err(fault("Hugging Face search rejected"));
    }
    let data: Value = response
        .json()
        .await
        .map_err(|_| fault("invalid search response"))?;
    let results = if let Some(rows) = data.as_array() {
        rows.iter().map(repository).collect::<Result<_, _>>()?
    } else {
        vec![repository(&data)?]
    };
    Ok(ManagerReply {
        status: "completed".into(),
        results,
        ..Default::default()
    })
}

#[cfg(test)]
#[path = "router_tests.rs"]
mod tests;
