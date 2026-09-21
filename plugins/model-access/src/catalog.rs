//! Source-scoped remote overlays never mutate an already resolved run target.
use eden_plugin_sdk::{CallContext, Package};
use eden_protocol::{Fault, models::*};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

const BUNDLED: &str = include_str!("../data/pi-models.json");
#[derive(Default, Deserialize)]
#[serde(default)]
struct ProviderOverride {
    base_url: Option<String>,
    api: Option<String>,
    headers: Option<BTreeMap<String, String>>,
    compat: Option<Value>,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct Config {
    #[serde(skip)]
    legacy: bool,
    #[serde(skip)]
    routing: Value,
    source: Option<String>,
    cache_path: Option<PathBuf>,
    offline: bool,
    models: Vec<ModelTarget>,
    providers: BTreeMap<String, ProviderOverride>,
    allowed_models: Option<Vec<String>>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Cached {
    models: Vec<Value>,
    etag: Option<String>,
    updated_at: u64,
}
#[derive(Default, Serialize, Deserialize)]
struct Disk {
    #[serde(default)]
    selected_source: Option<String>,
    sources: BTreeMap<String, BTreeMap<String, Cached>>,
    default: Option<ModelSelection>,
}
struct Inner {
    source: String,
    generation: u64,
    disk: Disk,
}
struct Catalog {
    config: Config,
    inner: Mutex<Inner>,
}
fn fault(message: &str) -> Fault {
    Fault::new("CatalogFailure", "model-access", message)
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn private_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if name.starts_with("x-eden-private-") {
        return true;
    }
    [
        "authorization",
        "cookie",
        "token",
        "api-key",
        "apikey",
        "secret",
    ]
    .iter()
    .any(|part| name.contains(part))
}
fn validate_target(target: &ModelTarget) -> Result<(), Fault> {
    if [&target.provider, &target.model, &target.api]
        .iter()
        .any(|s| s.trim().is_empty())
    {
        return Err(fault(
            "explicit model requires provider, model, and API identity",
        ));
    }
    validate_source(&target.base_url)?;
    if target.headers.keys().any(|name| private_header(name)) {
        return Err(fault(
            "authentication headers belong in private credentials configuration",
        ));
    }
    if target.headers.iter().any(|(name, value)| {
        reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err()
            || reqwest::header::HeaderValue::from_str(value).is_err()
    }) {
        return Err(fault("invalid explicit model headers"));
    }
    if target.limits.context_window > 0
        && u64::from(target.limits.max_output_tokens) > target.limits.context_window
    {
        return Err(fault("model output limit exceeds context window"));
    }
    Ok(())
}
fn validate_source(source: &str) -> Result<(), Fault> {
    let url = reqwest::Url::parse(source).map_err(|_| fault("invalid catalog source URL"))?;
    if !["http", "https"].contains(&url.scheme())
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(fault(
            "catalog source requires HTTP(S) without URL credentials",
        ));
    }
    Ok(())
}
pub(crate) fn register(package: Package, value: &Value) -> Result<Package, Fault> {
    let mut config: Config = serde_json::from_value(
        value
            .get("catalog")
            .cloned()
            .unwrap_or(serde_json::json!({})),
    )
    .map_err(|_| fault("invalid catalog configuration"))?;
    config.routing = value.clone();
    if config.cache_path.is_none() {
        config.cache_path = value
            .get("global_dir")
            .and_then(Value::as_str)
            .map(|p| PathBuf::from(p).join("model-catalog.json"));
    }
    config.legacy = ["model", "endpoint", "profile"]
        .iter()
        .any(|k| value.get(k).is_some())
        || std::env::var_os("OPENAI_MODEL").is_some();
    let source = config
        .source
        .clone()
        .unwrap_or_else(|| "https://pi.dev".into());
    validate_source(&source)?;
    let disk: Disk = config
        .cache_path
        .as_ref()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let source = config
        .source
        .clone()
        .or_else(|| disk.selected_source.clone())
        .unwrap_or(source);
    validate_source(&source)?;
    let state = Arc::new(Catalog {
        config,
        inner: Mutex::new(Inner {
            source,
            generation: 0,
            disk,
        }),
    });
    Ok(
        package.service(MODEL_CATALOG, move |request: CatalogRequest, cx| {
            let state = state.clone();
            async move { state.handle(request, cx).await }
        }),
    )
}
fn flatten(value: &Value) -> Vec<Value> {
    if value.get("id").and_then(Value::as_str).is_some() {
        return vec![value.clone()];
    }
    match value {
        Value::Array(a) => a.iter().flat_map(flatten).collect(),
        Value::Object(o) => o.values().flat_map(flatten).collect(),
        _ => vec![],
    }
}
fn target(provider: &str, v: &Value, source: CatalogSource) -> Result<ModelTarget, Fault> {
    let string = |key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| fault("catalog model is missing routing identity"))
    };
    let base_url = string("baseUrl")?;

    let headers: BTreeMap<String, String> =
        serde_json::from_value(v.get("headers").cloned().unwrap_or(serde_json::json!({})))
            .map_err(|_| fault("invalid catalog model headers"))?;
    // Authentication headers are resolved privately, never persisted with a target.
    if headers.keys().any(|k| private_header(k)) {
        return Err(fault("catalog contains a private authentication header"));
    }
    let mut compat = v
        .get("compat")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(levels) = v.get("thinkingLevelMap") {
        compat["thinkingLevelMap"] = levels.clone();
    }
    Ok(ModelTarget {
        provider: provider.into(),
        model: string("id")?,
        api: string("api")?,
        base_url,
        headers,
        limits: eden_protocol::coding::ModelLimits {
            context_window: v["contextWindow"].as_u64().unwrap_or(0),
            max_output_tokens: v["maxTokens"].as_u64().unwrap_or(0).min(u32::MAX as u64) as u32,
        },
        capabilities: ModelCapabilities {
            images: v["input"]
                .as_array()
                .is_some_and(|a| a.iter().any(|i| i == "image")),
            tools: true,
            reasoning: v["reasoning"].as_bool().unwrap_or(false),
        },
        pricing: v.get("cost").map(|c| ModelPricing {
            tiers: c["tiers"]
                .as_array()
                .map(|tiers| {
                    tiers
                        .iter()
                        .map(|t| ModelPricingTier {
                            input_tokens_above: t["inputTokensAbove"].as_u64().unwrap_or(0),
                            input: t["input"].as_f64(),
                            output: t["output"].as_f64(),
                            cache_read: t["cacheRead"].as_f64(),
                            cache_write: t["cacheWrite"].as_f64(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            input: c["input"].as_f64(),
            output: c["output"].as_f64(),
            cache_read: c["cacheRead"].as_f64(),
            cache_write: c["cacheWrite"].as_f64(),
            source: source.location.clone(),
        }),
        source,
        compat,
        ..Default::default()
    })
}
fn effective_thinking(target: &ModelTarget, requested: Option<&str>) -> Option<String> {
    let requested = requested?;
    if !target.capabilities.reasoning {
        return Some("off".into());
    }
    let map = target
        .compat
        .get("thinkingLevelMap")
        .and_then(Value::as_object);
    let levels = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
    let supported = |level: &str| match map.and_then(|map| map.get(level)) {
        Some(Value::Null) => false,
        Some(_) => true,
        None => !matches!(level, "xhigh" | "max"),
    };
    let index = levels.iter().position(|level| *level == requested)?;
    levels[index..]
        .iter()
        .chain(levels[..index].iter().rev())
        .find(|level| supported(level))
        .map(|level| (*level).to_owned())
        .or_else(|| Some("off".into()))
}
fn supported(api: &str) -> bool {
    matches!(
        api,
        "openai-responses"
            | "openai-codex-responses"
            | "pi-messages"
            | "openai-completions"
            | "anthropic-messages"
            | "azure-openai-responses"
            | "google-generative-ai"
            | "google-vertex"
            | "mistral-conversations"
            | "bedrock-converse-stream"
    )
}
enum Persist {
    Source,
    Default,
    Cache(String),
}
impl Catalog {
    async fn persist(&self, inner: &mut Inner, update: Persist) -> Result<(), Fault> {
        use std::io::Write;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        if let Some(path) = &self.config.cache_path {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|_| fault("cannot create catalog cache directory"))?;
            }
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path.with_extension("lock"))
                .map_err(|_| fault("cannot open catalog lock"))?;
            lock.lock()
                .map_err(|_| fault("cannot lock catalog cache"))?;
            let mut disk: Disk = if path.exists() {
                serde_json::from_slice(
                    &std::fs::read(path).map_err(|_| fault("cannot read catalog cache"))?,
                )
                .map_err(|_| fault("invalid catalog cache"))?
            } else {
                Disk::default()
            };
            match update {
                Persist::Source => disk.selected_source = inner.disk.selected_source.clone(),
                Persist::Default => disk.default = inner.disk.default.clone(),
                Persist::Cache(source) => {
                    if let Some(cache) = inner.disk.sources.get(&source) {
                        disk.sources.insert(source, cache.clone());
                    }
                }
            }
            let temporary = path.with_extension(format!(
                "{}.{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let bytes =
                serde_json::to_vec(&disk).map_err(|_| fault("cannot serialize catalog cache"))?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|_| fault("cannot create catalog cache"))?;
            if file
                .write_all(&bytes)
                .and_then(|()| file.sync_all())
                .and_then(|()| std::fs::rename(&temporary, path))
                .is_err()
            {
                let _ = std::fs::remove_file(&temporary);
                return Err(fault("cannot persist catalog cache"));
            }
            inner.disk = disk;
        }
        Ok(())
    }
    #[cfg(test)]
    fn entries(&self, inner: &Inner) -> Result<Vec<CatalogEntry>, Fault> {
        self.entries_scoped(inner, &BTreeMap::new())
    }
    fn entries_scoped(
        &self,
        inner: &Inner,
        scopes: &BTreeMap<String, String>,
    ) -> Result<Vec<CatalogEntry>, Fault> {
        let bundled: BTreeMap<String, Value> =
            serde_json::from_str(BUNDLED).map_err(|_| fault("invalid bundled catalog"))?;
        let mut models = BTreeMap::new();
        for (provider, values) in bundled {
            for v in flatten(&values) {
                let t = target(
                    &provider,
                    &v,
                    CatalogSource {
                        kind: "bundled".into(),
                        location: "pi-ai@0.85.1".into(),
                        updated_at: None,
                    },
                )?;
                models.insert(
                    (provider.clone(), t.model.clone()),
                    (t, v["name"].as_str().unwrap_or("").to_owned()),
                );
            }
        }
        if let Some(cached) = inner.disk.sources.get(&inner.source) {
            for (provider, cache) in cached {
                for v in &cache.models {
                    let t = target(
                        provider,
                        v,
                        CatalogSource {
                            kind: "remote".into(),
                            location: inner.source.clone(),
                            updated_at: Some(cache.updated_at),
                        },
                    )?;
                    models.insert(
                        (provider.clone(), t.model.clone()),
                        (t, v["name"].as_str().unwrap_or("").to_owned()),
                    );
                }
            }
        }
        for provider in ["radius", "github-copilot"] {
            let key = format!(
                "subscription:{provider}:{}:{}",
                scopes.get(provider).map(String::as_str).unwrap_or("public"),
                self.subscription_source(provider)
            );
            if let Some(cache) = inner.disk.sources.get(&key).and_then(|c| c.get(provider)) {
                if provider == "github-copilot" {
                    models.retain(|(p, id), _| {
                        p != provider || cache.models.iter().any(|m| m["id"] == *id)
                    });
                } else {
                    for v in &cache.models {
                        let t = target(
                            provider,
                            v,
                            CatalogSource {
                                kind: "remote".into(),
                                location: self.subscription_source(provider),
                                updated_at: Some(cache.updated_at),
                            },
                        )?;
                        models.insert(
                            (provider.into(), t.model.clone()),
                            (t, v["name"].as_str().unwrap_or("").into()),
                        );
                    }
                }
            }
        }
        for ((provider, _), (target, _)) in &mut models {
            if let Some(overrides) = self.config.providers.get(provider) {
                if let Some(base_url) = &overrides.base_url {
                    validate_source(base_url)?;
                    target.base_url = base_url.clone();
                    if !target.compat.is_object() {
                        target.compat = serde_json::json!({});
                    }
                    target.compat["endpointExplicit"] = serde_json::json!(true);
                }
                if let Some(api) = &overrides.api {
                    target.api = api.clone();
                }
                if let Some(headers) = &overrides.headers {
                    if headers.keys().any(|k| private_header(k)) {
                        return Err(fault(
                            "authentication headers belong in private credentials configuration",
                        ));
                    }
                    target.headers.extend(headers.clone());
                }
                if let Some(compat) = &overrides.compat {
                    if !target.compat.is_object() {
                        target.compat = serde_json::json!({});
                    }
                    if let Some(fields) = compat.as_object() {
                        for (key, value) in fields {
                            target.compat[key] = value.clone();
                        }
                    }
                }
                target.source = CatalogSource {
                    kind: "explicit".into(),
                    location: "configuration".into(),
                    updated_at: None,
                };
            }
        }
        for t in &self.config.models {
            validate_target(t)?;
            let mut t = t.clone();
            if !t.compat.is_object() {
                t.compat = serde_json::json!({});
            }
            t.compat["endpointExplicit"] = serde_json::json!(true);
            if t.headers.keys().any(|k| private_header(k)) {
                return Err(fault(
                    "authentication headers belong in private credentials configuration",
                ));
            }
            t.source = CatalogSource {
                kind: "explicit".into(),
                location: "configuration".into(),
                updated_at: None,
            };
            models.insert(
                (t.provider.clone(), t.model.clone()),
                (t.clone(), t.model.clone()),
            );
        }
        Ok(models
            .into_values()
            .map(|(mut target, name)| {
                crate::routes::freeze(&mut target, &self.config.routing, |key| {
                    std::env::var(key).ok()
                });
                let status = if self.config.allowed_models.as_ref().is_some_and(|allowed| {
                    !allowed.contains(&format!("{}/{}", target.provider, target.model))
                }) {
                    "excluded"
                } else if supported(&target.api)
                    && (validate_source(&target.base_url).is_err()
                        || target.base_url.contains('{')
                        || (target.api == "bedrock-converse-stream"
                            && target.compat["region"].as_str().is_none()))
                {
                    "configuration_required"
                } else if supported(&target.api) {
                    "authentication_required"
                } else {
                    "unsupported_protocol"
                }
                .into();
                CatalogEntry {
                    target,
                    name,
                    status,
                }
            })
            .collect())
    }
    fn subscription_source(&self, provider: &str) -> String {
        self.config
            .providers
            .get(provider)
            .and_then(|p| p.base_url.clone())
            .or_else(|| {
                self.config.routing["credentials"]["oauth"][provider]["gateway"]
                    .as_str()
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| {
                if provider == "radius" {
                    "https://radius.pi.dev".into()
                } else {
                    "https://api.individual.githubcopilot.com".into()
                }
            })
    }
    async fn refresh_subscriptions(&self, cx: &CallContext) -> Result<bool, Fault> {
        if self.config.offline {
            return Ok(true);
        }
        let generation = self.inner.lock().await.generation;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|_| fault("catalog client initialization failed"))?;
        let mut success = true;
        for provider in ["radius", "github-copilot"] {
            let resolved = cx
                .call::<_, CredentialReply>(
                    CREDENTIAL_SOURCE,
                    &CredentialRequest {
                        provider: provider.into(),
                        explicit: None,
                        purpose: "catalog_refresh".into(),
                    },
                )
                .await;
            let credential = match resolved {
                Ok(credential) => credential,
                Err(_) => {
                    success = false;
                    continue;
                }
            };
            if credential.api_key.is_none() && credential.headers.is_empty() {
                continue;
            }
            let source = self.subscription_source(provider);
            let base = if self
                .config
                .providers
                .get(provider)
                .and_then(|p| p.base_url.as_ref())
                .is_some()
            {
                &source
            } else {
                credential.base_url.as_ref().unwrap_or(&source)
            };
            validate_source(base)?;
            let mut url = reqwest::Url::parse(base)
                .map_err(|_| fault("invalid subscription catalog endpoint"))?;
            if provider == "radius" {
                url.set_path("/v1/config");
            } else {
                url.set_path(&format!("{}/models", url.path().trim_end_matches('/')));
            }
            url.set_query(None);
            let mut request = client.get(url).header("Accept", "application/json");
            if provider == "github-copilot" {
                let target = ModelTarget {
                    provider: provider.into(),
                    ..Default::default()
                };
                let input = eden_protocol::coding::ModelInput {
                    target: None,
                    max_output_tokens: None,
                    items: vec![],
                    tools: vec![],
                };
                for (name, value) in crate::subscription::headers(&target, &input) {
                    request = request.header(name, value);
                }
            }
            if let Some(key) = &credential.api_key {
                request = request.bearer_auth(key);
            }
            for (name, value) in &credential.headers {
                request = request.header(name, value);
            }
            let fetch = async {
                let response = request
                    .send()
                    .await
                    .map_err(|_| fault("subscription catalog request failed"))?;
                if !response.status().is_success() {
                    return Err(fault("subscription catalog request rejected"));
                }
                let body = response
                    .json::<Value>()
                    .await
                    .map_err(|_| fault("invalid subscription catalog response"))?;
                if provider == "radius" {
                    crate::subscription::radius_models(&body)
                } else {
                    crate::subscription::copilot_models(&body)
                }
            };
            let cancellation = cx.scope.cancellation();
            let result = tokio::select! {
                _ = cancellation.cancelled() => return Err(fault("catalog refresh cancelled")),
                result = fetch => result,
            };
            let Ok(models) = result else {
                success = false;
                continue;
            };
            let mut inner = self.inner.lock().await;
            if inner.generation != generation {
                return Ok(false);
            }
            let source = format!(
                "subscription:{provider}:{}:{source}",
                crate::subscription::catalog_scope(&credential)
            );
            inner.disk.sources.insert(
                source.clone(),
                BTreeMap::from([(
                    provider.into(),
                    Cached {
                        models,
                        etag: None,
                        updated_at: now(),
                    },
                )]),
            );
            self.persist(&mut inner, Persist::Cache(source)).await?;
        }
        Ok(success)
    }
    async fn refresh(&self, cx: Option<&CallContext>) -> Result<String, Fault> {
        if self.config.offline {
            return Ok("offline".into());
        }
        let (source, generation, cached) = {
            let inner = self.inner.lock().await;
            (
                inner.source.clone(),
                inner.generation,
                inner
                    .disk
                    .sources
                    .get(&inner.source)
                    .cloned()
                    .unwrap_or_default(),
            )
        };
        let providers: BTreeMap<String, Value> =
            serde_json::from_str(BUNDLED).map_err(|_| fault("invalid bundled catalog"))?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(4))
            .build()
            .map_err(|_| fault("catalog HTTP client initialization failed"))?;
        let mut updates = cached.clone();
        let mut failed = false;
        for provider in providers.keys() {
            let mut url =
                reqwest::Url::parse(&source).map_err(|_| fault("invalid catalog source"))?;
            url.set_path(&format!("/api/models/providers/{provider}"));
            url.set_query(None);
            let mut req = client.get(url).header("Accept", "application/json");
            if let Some(etag) = cached
                .get(provider)
                .filter(|c| !c.models.is_empty())
                .and_then(|c| c.etag.as_ref())
            {
                req = req.header("If-None-Match", etag);
            }
            let response = if let Some(cx) = cx {
                let cancel = cx.scope.cancellation();
                tokio::select! {
                    _ = cancel.cancelled() => return Err(fault("catalog refresh cancelled")),
                    response = req.send() => response,
                }
            } else {
                req.send().await
            };
            let Ok(response) = response else {
                failed = true;
                continue;
            };
            if response.status() == 304 {
                if let Some(c) = updates.get_mut(provider) {
                    c.updated_at = now();
                } else {
                    failed = true;
                }
                continue;
            }
            if !response.status().is_success() {
                failed = true;
                continue;
            }
            let etag = response
                .headers()
                .get("etag")
                .and_then(|h| h.to_str().ok())
                .map(str::to_owned);
            let Ok(body) = response.json::<Value>().await else {
                failed = true;
                continue;
            };
            let models = flatten(&body);
            if models.is_empty()
                || models
                    .iter()
                    .any(|v| target(provider, v, CatalogSource::default()).is_err())
            {
                failed = true;
                continue;
            }
            updates.insert(
                provider.clone(),
                Cached {
                    models,
                    etag,
                    updated_at: now(),
                },
            );
        }
        let mut inner = self.inner.lock().await;
        if inner.generation != generation || inner.source != source {
            return Ok("superseded".into());
        }
        inner.disk.sources.insert(source.clone(), updates);
        self.persist(&mut inner, Persist::Cache(source)).await?;
        Ok(if failed {
            "refresh_failed_cached"
        } else {
            "refreshed"
        }
        .into())
    }
    async fn handle(
        &self,
        request: CatalogRequest,
        cx: CallContext,
    ) -> Result<CatalogReply, Fault> {
        let status = if matches!(request, CatalogRequest::Refresh) {
            let status = self.refresh(Some(&cx)).await?;
            if self.refresh_subscriptions(&cx).await? {
                status
            } else {
                "refresh_failed_cached".into()
            }
        } else {
            "available".into()
        };
        let mut scopes = BTreeMap::new();
        for provider in ["radius", "github-copilot"] {
            let credential: CredentialReply = cx
                .call(
                    CREDENTIAL_SOURCE,
                    &CredentialRequest {
                        provider: provider.into(),
                        explicit: None,
                        purpose: "catalog".into(),
                    },
                )
                .await?;
            scopes.insert(
                provider.into(),
                crate::subscription::catalog_scope(&credential),
            );
        }
        let mut inner = self.inner.lock().await;
        if let CatalogRequest::SetSource { url } = &request {
            validate_source(url)?;
            inner.source = url.clone();
            inner.generation += 1;
            inner.disk.selected_source = Some(url.clone());
            self.persist(&mut inner, Persist::Source).await?;
        }
        let mut models = self.entries_scoped(&inner, &scopes)?;
        if let Some(region) = crate::cloud::configured_region(&self.config.routing).await {
            for entry in &mut models {
                if entry.target.api == "bedrock-converse-stream"
                    && entry.target.compat["region"].as_str().is_none()
                {
                    entry.target.compat["region"] = serde_json::json!(region);
                    crate::routes::freeze(&mut entry.target, &self.config.routing, |key| {
                        std::env::var(key).ok()
                    });
                    if entry.status == "configuration_required"
                        && validate_source(&entry.target.base_url).is_ok()
                    {
                        entry.status = "authentication_required".into();
                    }
                }
            }
        }
        let mut auth = BTreeMap::new();
        let mut account_models: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut keyless = std::collections::BTreeSet::new();
        for entry in &mut models {
            if entry.status != "authentication_required" {
                continue;
            }
            if !auth.contains_key(&entry.target.provider) {
                let reply: CredentialReply = cx
                    .call(
                        CREDENTIAL_SOURCE,
                        &CredentialRequest {
                            provider: entry.target.provider.clone(),
                            explicit: None,
                            purpose: "catalog".into(),
                        },
                    )
                    .await?;
                if let Some(ids) = reply.available_model_ids {
                    account_models.insert(entry.target.provider.clone(), ids);
                }
                if reply.api_key.is_none() && reply.source != "command_configured" {
                    keyless.insert(entry.target.provider.clone());
                }
                auth.insert(
                    entry.target.provider.clone(),
                    reply.api_key.is_some()
                        || matches!(
                            reply.source.as_str(),
                            "command_configured"
                                | "headers_configured"
                                | "cloud_configured"
                                | "oauth_configured"
                        ),
                );
            }
            if account_models
                .get(&entry.target.provider)
                .is_some_and(|ids| !ids.contains(&entry.target.model))
                && entry.target.source.kind != "explicit"
            {
                entry.status = "unavailable_for_account".into();
                continue;
            }
            if auth[&entry.target.provider] {
                entry.status = if entry.target.api == "google-vertex"
                    && keyless.contains(&entry.target.provider)
                    && ["project", "location"]
                        .iter()
                        .any(|key| entry.target.compat[key].as_str().is_none_or(str::is_empty))
                {
                    "configuration_required"
                } else {
                    "configured"
                }
                .into();
            }
        }
        let selection = match &request {
            CatalogRequest::Resolve { selection } => selection
                .clone()
                .or_else(|| {
                    inner.disk.default.clone().filter(|default| {
                        models.iter().any(|e| {
                            e.status == "configured"
                                && e.target.provider == default.provider
                                && e.target.model == default.model
                        })
                    })
                })
                .or_else(|| {
                    if self.config.legacy && inner.disk.default.is_none() {
                        None
                    } else {
                        models
                            .iter()
                            .find(|e| e.status == "configured")
                            .map(|e| ModelSelection {
                                provider: e.target.provider.clone(),
                                model: e.target.model.clone(),
                                thinking: None,
                            })
                    }
                }),
            CatalogRequest::SetDefault { selection } => Some(selection.clone()),
            _ => None,
        };
        if selection.is_none()
            && matches!(request, CatalogRequest::Resolve { .. })
            && !self.config.legacy
        {
            return Err(Fault::new(
                "ModelUnavailable",
                "model-access",
                "no configured model is available; configure an API key and select a model",
            ));
        }
        let target = if let Some(selection) = selection {
            if selection.thinking.as_deref().is_some_and(|t| {
                !matches!(
                    t,
                    "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
                )
            }) {
                return Err(fault("unsupported thinking level"));
            }
            let entry = models.iter().find(|e| {
                e.target.provider == selection.provider && e.target.model == selection.model
            });
            if entry.is_some_and(|e| e.status == "configuration_required") {
                return Err(fault(
                    "selected model requires cloud endpoint, project, location or region \
                     configuration",
                ));
            }
            if entry.is_some_and(|e| e.status == "unavailable_for_account") {
                return Err(Fault::new(
                    "ModelUnavailable",
                    "model-access",
                    "selected model is unavailable for the authenticated account",
                ));
            }
            if entry.is_some_and(|e| e.status == "excluded") {
                return Err(Fault::new(
                    "ModelUnavailable",
                    "model-access",
                    "selected model is outside the configured model set",
                ));
            }
            let mut target = models
                .iter()
                .find(|e| {
                    e.target.provider == selection.provider && e.target.model == selection.model
                })
                .ok_or_else(|| {
                    Fault::new(
                        "ModelUnavailable",
                        "model-access",
                        "selected model is absent from catalog",
                    )
                })?
                .target
                .clone();
            if !auth.get(&target.provider).copied().unwrap_or(false) && supported(&target.api) {
                return Err(Fault::new(
                    "ModelUnavailable",
                    "model-access",
                    "selected model requires authentication",
                ));
            }
            if !supported(&target.api) {
                return Err(fault("selected model protocol is not implemented"));
            }
            validate_source(&target.base_url)?;
            target.thinking = ThinkingSelection {
                requested: selection.thinking.clone(),
                effective: effective_thinking(&target, selection.thinking.as_deref()),
            };
            if matches!(request, CatalogRequest::SetDefault { .. }) {
                inner.disk.default = Some(selection);
                self.persist(&mut inner, Persist::Default).await?;
            }
            Some(target)
        } else {
            None
        };
        let mut providers: Vec<String> = models.iter().map(|e| e.target.provider.clone()).collect();
        providers.push("radius".into());
        providers.sort();
        providers.dedup();
        Ok(CatalogReply {
            providers,
            models,
            target,
            source: CatalogSource {
                kind: "catalog".into(),
                location: inner.source.clone(),
                updated_at: None,
            },
            status,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(source: String, path: Option<PathBuf>) -> Catalog {
        Catalog {
            config: Config {
                cache_path: path,
                ..Default::default()
            },
            inner: Mutex::new(Inner {
                source,
                generation: 0,
                disk: Disk::default(),
            }),
        }
    }
    #[tokio::test]
    async fn refresh_persists_source_scoped_cache_and_preserves_it_on_304_and_bad_json() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source = format!("http://{}", listener.local_addr().unwrap());
        let phase = Arc::new(AtomicUsize::new(0));
        let state = phase.clone();
        let server = tokio::spawn(async move {
            for _ in 0..117 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = vec![0; 8192];
                let size = stream.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..size]);
                let current = state.load(Ordering::SeqCst);
                let (status, body) = match current {
                    0 => (
                        "200 OK",
                        r#"[{"id":"remote-model","name":"Remote","api":"openai-responses","baseUrl":"http://localhost/v1","contextWindow":777}]"#,
                    ),
                    1 => {
                        assert!(
                            request
                                .to_ascii_lowercase()
                                .contains("if-none-match: fixture")
                        );
                        ("304 Not Modified", "")
                    }
                    _ => ("200 OK", "broken"),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nETag: fixture\r\nConnection: \
                     close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let path = std::env::temp_dir().join(format!("eden-catalog-{}.json", std::process::id()));
        let catalog = fixture(source.clone(), Some(path.clone()));
        assert_eq!(catalog.refresh(None).await.unwrap(), "refreshed");
        phase.store(1, Ordering::SeqCst);
        assert_eq!(catalog.refresh(None).await.unwrap(), "refreshed");
        phase.store(2, Ordering::SeqCst);
        assert_eq!(
            catalog.refresh(None).await.unwrap(),
            "refresh_failed_cached"
        );
        server.await.unwrap();
        let disk: Disk = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let inner = Inner {
            source,
            generation: 0,
            disk,
        };
        assert!(
            catalog
                .entries(&inner)
                .unwrap()
                .iter()
                .any(|e| e.target.model == "remote-model" && e.target.limits.context_window == 777)
        );
        let switched = Inner {
            source: "http://different.invalid".into(),
            ..inner
        };
        assert!(
            !catalog
                .entries(&switched)
                .unwrap()
                .iter()
                .any(|e| e.target.model == "remote-model")
        );
        std::fs::remove_file(path).unwrap();
    }
    #[tokio::test]
    async fn offline_refresh_does_not_connect() {
        let mut catalog = fixture("http://127.0.0.1:1".into(), None);
        catalog.config.offline = true;
        assert_eq!(catalog.refresh(None).await.unwrap(), "offline");
    }
    #[tokio::test]
    async fn source_change_discards_late_publication() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source = format!("http://{}", listener.local_addr().unwrap());
        let (entered, seen) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut entered = Some(entered);
            let mut released = Some(released);
            for _ in 0..39 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = [0; 8192];
                assert!(stream.read(&mut bytes).await.unwrap() > 0);
                if let Some(entered) = entered.take() {
                    entered.send(()).unwrap();
                    released.take().unwrap().await.unwrap();
                }
                let body =
                    r#"[{"id":"late","api":"openai-responses","baseUrl":"http://localhost"}]"#;
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: \
                             close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        let catalog = Arc::new(fixture(source, None));
        let worker = catalog.clone();
        let refresh = tokio::spawn(async move { worker.refresh(None).await });
        seen.await.unwrap();
        {
            let mut inner = catalog.inner.lock().await;
            inner.source = "http://new.invalid".into();
            inner.generation += 1;
        }
        release.send(()).unwrap();
        assert_eq!(refresh.await.unwrap().unwrap(), "superseded");
        assert!(catalog.inner.lock().await.disk.sources.is_empty());
        server.await.unwrap();
    }
    #[tokio::test]
    async fn stale_refresh_preserves_another_instances_saved_default() {
        let dir = std::env::temp_dir().join(format!("eden-catalog-merge-{}", std::process::id()));
        let path = dir.join("catalog.json");
        let first = fixture("http://source".into(), Some(path.clone()));
        let second = fixture("http://source".into(), Some(path));
        let mut a = first.inner.lock().await;
        let mut b = second.inner.lock().await;
        b.disk.default = Some(ModelSelection {
            provider: "deepseek".into(),
            model: "deepseek-v4-flash".into(),
            thinking: None,
        });
        second.persist(&mut b, Persist::Default).await.unwrap();
        a.disk
            .sources
            .insert("http://source".into(), BTreeMap::new());
        first
            .persist(&mut a, Persist::Cache("http://source".into()))
            .await
            .unwrap();
        assert_eq!(a.disk.default.as_ref().unwrap().provider, "deepseek");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn allowed_model_set_marks_other_models_excluded() {
        let mut catalog = fixture("http://source".into(), None);
        catalog.config.allowed_models = Some(vec!["deepseek/deepseek-v4-flash".into()]);
        let inner = Inner {
            source: "http://source".into(),
            generation: 0,
            disk: Disk::default(),
        };
        let entries = catalog.entries(&inner).unwrap();
        assert!(
            entries
                .iter()
                .filter(|e| e.status != "excluded")
                .all(|e| e.target.provider == "deepseek" && e.target.model == "deepseek-v4-flash")
        );
        assert!(entries.iter().any(|e| e.status == "excluded"));
    }
    #[test]
    fn thinking_sparse_maps_preserve_off_and_clamp_disabled_levels_upward() {
        let mut target = ModelTarget {
            capabilities: ModelCapabilities {
                reasoning: true,
                ..Default::default()
            },
            compat: serde_json::json!({ "thinkingLevelMap": { "xhigh": null } }),
            ..Default::default()
        };
        assert_eq!(
            effective_thinking(&target, Some("off")).as_deref(),
            Some("off")
        );
        assert_eq!(
            effective_thinking(&target, Some("medium")).as_deref(),
            Some("medium")
        );
        assert_eq!(
            effective_thinking(&target, Some("xhigh")).as_deref(),
            Some("high")
        );
        target.compat = serde_json::json!({
            "thinkingLevelMap": {
                "minimal": null,
                "low": null,
                "medium": null,
                "high": "high",
                "max": "max",
            },
        });
        assert_eq!(
            effective_thinking(&target, Some("medium")).as_deref(),
            Some("high")
        );
        target.capabilities.reasoning = false;
        assert_eq!(
            effective_thinking(&target, Some("high")).as_deref(),
            Some("off")
        );
    }
    #[test]
    fn explicit_model_cannot_publish_authentication_headers_or_invalid_routes() {
        let mut catalog = fixture("http://source".into(), None);
        let target = ModelTarget {
            provider: "custom".into(),
            model: "custom".into(),
            api: "openai-responses".into(),
            base_url: "http://localhost".into(),
            headers: BTreeMap::from([("Authorization".into(), "canary-secret".into())]),
            ..Default::default()
        };
        catalog.config.models.push(target.clone());
        let inner = Inner {
            source: "http://source".into(),
            generation: 0,
            disk: Disk::default(),
        };
        let error = catalog.entries(&inner).unwrap_err();
        assert!(!error.message.contains("canary"));
        let mut invalid = target;
        invalid.headers.clear();
        invalid.base_url = "file:///secret".into();
        assert!(validate_target(&invalid).is_err());
        invalid.base_url = "http://localhost".into();
        invalid.limits.context_window = 10;
        invalid.limits.max_output_tokens = 11;
        assert!(validate_target(&invalid).is_err());
    }
    #[test]
    fn explicit_overrides_remote_and_thinking_retains_supported_effective_level() {
        let mut catalog = fixture("http://source".into(), None);
        catalog.config.providers.insert(
            "deepseek".into(),
            ProviderOverride {
                base_url: Some("http://explicit".into()),
                ..Default::default()
            },
        );
        let mut disk = Disk::default();
        disk.sources.insert(
            "http://source".into(),
            BTreeMap::from([(
                "deepseek".into(),
                Cached {
                    models: vec![serde_json::json!({
                        "id": "deepseek-v4-flash",
                        "api": "openai-completions",
                        "baseUrl": "http://remote",
                        "reasoning": true,
                        "thinkingLevelMap": { "low": "low", "medium": null, "high": "high" },
                    })],
                    ..Default::default()
                },
            )]),
        );
        let entries = catalog
            .entries(&Inner {
                source: "http://source".into(),
                generation: 0,
                disk,
            })
            .unwrap();
        let target = &entries
            .iter()
            .find(|e| e.target.provider == "deepseek" && e.target.model == "deepseek-v4-flash")
            .unwrap()
            .target;
        assert_eq!(target.base_url, "http://explicit");
        assert_eq!(
            effective_thinking(target, Some("medium")).as_deref(),
            Some("high")
        );
    }
    #[test]
    fn bundled_routes_preserve_provider_identity_and_reject_secret_headers() {
        let disk = Disk::default();
        let inner = Inner {
            source: "https://pi.dev".into(),
            generation: 0,
            disk,
        };
        let catalog = Catalog {
            config: Config::default(),
            inner: Mutex::new(Inner {
                source: String::new(),
                generation: 0,
                disk: Disk::default(),
            }),
        };
        let entries = catalog.entries(&inner).unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.target.provider == "deepseek" && e.target.api == "openai-completions")
        );
        assert!(entries.iter().all(|e| e.status != "unsupported_protocol"));
        assert!(!supported("unknown-custom-wire"));
        assert!(
            target(
                "x",
                &serde_json::json!({
                    "id": "x",
                    "api": "openai-responses",
                    "baseUrl": "http://localhost",
                    "headers": { "Authorization": "secret" },
                }),
                CatalogSource::default()
            )
            .is_err()
        );
    }
    #[tokio::test]
    async fn radius_cache_is_durable_gateway_scoped_and_explicit_models_win() {
        let path =
            std::env::temp_dir().join(format!("eden-radius-catalog-{}.json", std::process::id()));
        let catalog = fixture("https://pi.dev".into(), Some(path.clone()));
        let gateway = format!(
            "subscription:radius:public:{}",
            catalog.subscription_source("radius")
        );
        let models = crate::subscription::radius_models(&serde_json::json!({
            "baseUrl": "https://inference.test/v1",
            "models": [{
                "id": "radius-test",
                "name": "Radius Test",
                "reasoning": true,
                "input": ["text"],
                "cost": {},
                "contextWindow": 8192,
                "maxTokens": 1024,
            }],
        }))
        .unwrap();
        let mut inner = catalog.inner.lock().await;
        inner.disk.sources.insert(
            gateway.clone(),
            BTreeMap::from([(
                "radius".into(),
                Cached {
                    models,
                    etag: None,
                    updated_at: 1,
                },
            )]),
        );
        catalog
            .persist(&mut inner, Persist::Cache(gateway))
            .await
            .unwrap();
        let disk: Disk = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let reread = Inner {
            source: "https://pi.dev".into(),
            generation: 0,
            disk,
        };
        assert!(
            !catalog
                .entries_scoped(
                    &reread,
                    &BTreeMap::from([("radius".into(), "other-account".into())])
                )
                .unwrap()
                .iter()
                .any(|entry| entry.target.provider == "radius")
        );
        let entries = catalog.entries(&reread).unwrap();
        let radius = entries
            .iter()
            .find(|e| e.target.provider == "radius")
            .unwrap();
        assert_eq!(radius.target.base_url, "https://inference.test/v1");
        assert_eq!(radius.target.api, "pi-messages");
        let mut different = fixture("https://pi.dev".into(), None);
        different.config.providers.insert(
            "radius".into(),
            ProviderOverride {
                base_url: Some("https://other.test".into()),
                ..Default::default()
            },
        );
        assert!(
            !different
                .entries(&reread)
                .unwrap()
                .iter()
                .any(|e| e.target.provider == "radius")
        );
        let mut explicit = radius.target.clone();
        explicit.base_url = "https://explicit.test".into();
        different.config.providers.clear();
        different.config.models.push(explicit);
        assert_eq!(
            different
                .entries(&reread)
                .unwrap()
                .iter()
                .find(|e| e.target.provider == "radius")
                .unwrap()
                .target
                .base_url,
            "https://explicit.test"
        );
        drop(inner);
        let _ = std::fs::remove_file(path.with_extension("lock"));
        let _ = std::fs::remove_file(path);
    }
}
