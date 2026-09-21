//! Provider-specific exchanges share owned HTTP futures and a private token representation.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use eden_protocol::{
    Fault,
    models::{AuthInteraction, CredentialReply},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

pub(crate) fn fault(message: &str) -> Fault {
    Fault::new("OAuthFailure", "model-access", message)
}
pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Config {
    pub client_id: Option<String>,
    pub authorization_url: Option<String>,
    pub token_url: Option<String>,
    pub device_url: Option<String>,
    pub device_token_url: Option<String>,
    pub redirect_uri: Option<String>,
    pub scope: Option<String>,
    pub gateway: Option<String>,
    pub domain: Option<String>,
    pub copilot_token_url: Option<String>,
    pub headers: BTreeMap<String, String>,
}
// The issuer and client are saved with the credential so a configuration reload cannot
// accidentally send an existing refresh token to a different authorization server.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Settings {
    provider: String,
    client_id: String,
    authorization_url: String,
    token_url: String,
    device_url: String,
    device_token_url: String,
    redirect_uri: String,
    scope: String,
    copilot_token_url: String,
    domain: String,
    headers: BTreeMap<String, String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Token {
    settings: Settings,
    access: String,
    refresh: String,
    pub expires_at: u64,
    base_url: Option<String>,
    account_id: Option<String>,
    catalog_scope: String,
    available_model_ids: Option<Vec<String>>,
}
pub(crate) struct Flow {
    settings: Settings,
    verifier: String,
    state: String,
    listener: Option<TcpListener>,
    device: Option<Value>,
    pub interaction: AuthInteraction,
    method: String,
}
pub(crate) fn random() -> Result<String, Fault> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| fault("cannot obtain OAuth randomness"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn url(value: &str) -> Result<reqwest::Url, Fault> {
    let parsed = reqwest::Url::parse(value).map_err(|_| fault("invalid OAuth endpoint"))?;
    let local = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if (parsed.scheme() != "https" && !(parsed.scheme() == "http" && local))
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(fault(
            "OAuth endpoints require HTTPS (HTTP is allowed only for loopback)",
        ));
    }
    Ok(parsed)
}
fn client() -> Result<reqwest::Client, Fault> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| fault("cannot initialize OAuth HTTP client"))
}
async fn response(request: reqwest::RequestBuilder) -> Result<(u16, Value), Fault> {
    let response = request
        .send()
        .await
        .map_err(|_| fault("OAuth network request failed"))?;
    let status = response.status().as_u16();
    // Never embed URLs, response bodies, or reqwest errors in public failures.
    let bytes = response
        .bytes()
        .await
        .map_err(|_| fault("OAuth response interrupted"))?;
    if bytes.len() > 1024 * 1024 {
        return Err(fault("OAuth response exceeds limit"));
    }
    let value =
        serde_json::from_slice(&bytes).map_err(|_| fault("OAuth response is not valid JSON"))?;
    Ok((status, value))
}
async fn post(
    settings: &Settings,
    endpoint: &str,
    fields: Value,
    json_body: bool,
) -> Result<(u16, Value), Fault> {
    let mut request = client()?
        .post(url(endpoint)?)
        .header("accept", "application/json");
    for (name, value) in &settings.headers {
        request = request.header(name, value);
    }
    if json_body {
        request = request.json(&fields);
    } else {
        let mut encoded = reqwest::Url::parse("http://localhost/")
            .map_err(|_| fault("cannot encode OAuth request"))?;
        for (key, value) in fields
            .as_object()
            .ok_or_else(|| fault("invalid OAuth fields"))?
        {
            if let Some(value) = value.as_str() {
                encoded.query_pairs_mut().append_pair(key, value);
            }
        }
        request = request
            .header("content-type", "application/x-www-form-urlencoded")
            .body(encoded.query().unwrap_or_default().to_owned());
    }
    response(request).await
}
fn required(value: &Value, key: &str) -> Result<String, Fault> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| fault("OAuth response is missing a required field"))
}
fn successful(status: u16) -> Result<(), Fault> {
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(fault(&format!(
            "OAuth endpoint rejected request (HTTP {status}); check client admission and account \
             access"
        )))
    }
}
impl Settings {
    fn new(provider: &str, config: &Config) -> Result<Self, Fault> {
        let (authorization, token, device, redirect, scope) = match provider {
            "anthropic" => (
                "https://claude.ai/oauth/authorize",
                "https://platform.claude.com/v1/oauth/token",
                "",
                "http://localhost:53692/callback",
                "org:create_api_key user:profile user:inference user:sessions:claude_code \
                 user:mcp_servers user:file_upload",
            ),
            "openai-codex" => (
                "https://auth.openai.com/oauth/authorize",
                "https://auth.openai.com/oauth/token",
                "https://auth.openai.com/api/accounts/deviceauth/usercode",
                "http://localhost:1455/auth/callback",
                "openid profile email offline_access",
            ),
            "github-copilot" => (
                "",
                "https://github.com/login/oauth/access_token",
                "https://github.com/login/device/code",
                "",
                "read:user",
            ),
            "xai" => (
                "",
                "https://auth.x.ai/oauth2/token",
                "https://auth.x.ai/oauth2/device/code",
                "",
                "openid profile email offline_access grok-cli:access api:access",
            ),
            "kimi-coding" => (
                "",
                "https://auth.kimi.com/api/oauth/token",
                "https://auth.kimi.com/api/oauth/device_authorization",
                "",
                "",
            ),
            "openrouter" => (
                "https://openrouter.ai/auth",
                "https://openrouter.ai/api/v1/auth/keys",
                "",
                "http://127.0.0.1:0/oauth/callback",
                "",
            ),
            "radius" => (
                "",
                "https://radius.pi.dev/v1/oauth/token",
                "https://radius.pi.dev/v1/oauth/device",
                "http://127.0.0.1:1456/oauth/callback",
                "gateway offline_access",
            ),
            _ => return Err(fault("provider does not support OAuth login")),
        };
        let client_id = if provider == "openrouter" {
            String::new()
        } else {
            config
                .client_id
                .clone()
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| {
                    fault(
                        "OAuth client admission is not configured; set \
                         credentials.oauth.<provider>.client_id to an authorized client",
                    )
                })?
        };
        let domain = config.domain.clone().unwrap_or_else(|| "github.com".into());
        if domain.contains('/') || domain.contains(':') || domain.is_empty() {
            return Err(fault("invalid GitHub Enterprise domain"));
        }
        let gateway = config
            .gateway
            .as_deref()
            .unwrap_or("https://radius.pi.dev")
            .trim_end_matches('/');
        let mut settings = Self {
            provider: provider.into(),
            client_id,
            authorization_url: authorization.into(),
            token_url: token.into(),
            device_url: device.into(),
            device_token_url: "https://auth.openai.com/api/accounts/deviceauth/token".into(),
            redirect_uri: redirect.into(),
            scope: scope.into(),
            copilot_token_url: if domain == "github.com" {
                "https://api.github.com/copilot_internal/v2/token".into()
            } else {
                format!("https://api.{domain}/copilot_internal/v2/token")
            },
            domain,
            headers: config.headers.clone(),
        };
        if provider == "radius" {
            settings.token_url = format!("{gateway}/v1/oauth/token");
            settings.device_url = format!("{gateway}/v1/oauth/device");
            settings.authorization_url = format!("{gateway}/v1/oauth");
        }
        if provider == "github-copilot" {
            settings.device_url = format!("https://{}/login/device/code", settings.domain);
            settings.token_url = format!("https://{}/login/oauth/access_token", settings.domain);
        }
        macro_rules! overrides { ($($field:ident),*) => { $(if let Some(value) = &config.$field { settings.$field = value.clone(); })* }; }
        overrides!(
            authorization_url,
            token_url,
            device_url,
            device_token_url,
            redirect_uri,
            scope,
            copilot_token_url
        );
        Ok(settings)
    }
}
impl Flow {
    pub async fn start(
        provider: &str,
        method: Option<&str>,
        config: &Config,
    ) -> Result<Self, Fault> {
        let mut settings = Settings::new(provider, config)?;
        let method = method.unwrap_or(
            if matches!(provider, "xai" | "kimi-coding" | "github-copilot") {
                "device"
            } else {
                "browser"
            },
        );
        if !matches!(method, "browser" | "device")
            || (method == "device" && matches!(provider, "anthropic" | "openrouter"))
            || (method == "browser" && matches!(provider, "xai" | "kimi-coding" | "github-copilot"))
        {
            return Err(fault("login method is not supported by this provider"));
        }
        let verifier = random()?;
        let state = random()?;
        let mut flow = Self {
            settings: settings.clone(),
            verifier,
            state,
            listener: None,
            device: None,
            interaction: AuthInteraction::default(),
            method: method.into(),
        };
        if method == "device" {
            let (status, device) = post(
                &settings,
                &settings.device_url,
                json!({ "client_id": settings.client_id, "scope": settings.scope }),
                provider == "openai-codex",
            )
            .await?;
            successful(status)?;
            required(
                &device,
                if provider == "openai-codex" {
                    "device_auth_id"
                } else {
                    "device_code"
                },
            )?;
            let user_code = required(&device, "user_code")?;
            let uri = if provider == "openai-codex" {
                "https://auth.openai.com/codex/device".into()
            } else {
                device
                    .get("verification_uri_complete")
                    .or_else(|| device.get("verification_uri"))
                    .or_else(|| device.get("verification_url"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| fault("device verification URL is missing"))?
                    .to_owned()
            };
            url(&uri)?;
            flow.interaction = AuthInteraction {
                url: uri,
                user_code: Some(user_code),
                manual_input: false,
                expires_at: now().saturating_add(
                    device
                        .get("expires_in")
                        .and_then(Value::as_u64)
                        .unwrap_or(900),
                ),
            };
            flow.device = Some(device);
        } else {
            let mut redirect = url(&settings.redirect_uri)?;
            if redirect.scheme() != "http"
                || !matches!(redirect.host_str(), Some("localhost" | "127.0.0.1"))
            {
                return Err(fault(
                    "OAuth callback must use an IPv4 loopback HTTP address",
                ));
            }
            let listener =
                TcpListener::bind(("127.0.0.1", redirect.port_or_known_default().unwrap_or(80)))
                    .await
                    .map_err(|_| {
                        fault(
                            "OAuth callback port is unavailable; close the other login or \
                             configure an admitted redirect_uri",
                        )
                    })?;
            let port = listener
                .local_addr()
                .map_err(|_| fault("cannot inspect callback address"))?
                .port();
            redirect
                .set_port(Some(port))
                .map_err(|_| fault("invalid callback port"))?;
            if provider == "openrouter" {
                redirect.set_path(&format!("/oauth/{}", random()?));
            }
            settings.redirect_uri = redirect.into();
            if provider == "radius" {
                let (status, discovery) =
                    response(client()?.get(url(&settings.authorization_url)?)).await?;
                successful(status)?;
                settings.authorization_url = required(&discovery, "authorizationEndpoint")?;
            }
            let mut authorization = url(&settings.authorization_url)?;
            {
                let mut query = authorization.query_pairs_mut();
                query
                    .append_pair(
                        "code_challenge",
                        &URL_SAFE_NO_PAD.encode(Sha256::digest(flow.verifier.as_bytes())),
                    )
                    .append_pair("code_challenge_method", "S256");
                if provider == "openrouter" {
                    query.append_pair("callback_url", &settings.redirect_uri);
                } else {
                    query
                        .append_pair("client_id", &settings.client_id)
                        .append_pair("redirect_uri", &settings.redirect_uri)
                        .append_pair("response_type", "code")
                        .append_pair("scope", &settings.scope)
                        .append_pair("state", &flow.state);
                    if provider == "anthropic" {
                        query.append_pair("code", "true");
                    }
                    if provider == "openai-codex" {
                        query.append_pair("id_token_add_organizations", "true");
                    }
                }
            }
            flow.interaction = AuthInteraction {
                url: authorization.into(),
                user_code: None,
                manual_input: provider != "radius",
                expires_at: now() + 600,
            };
            flow.listener = Some(listener);
            flow.settings = settings;
        }
        Ok(flow)
    }
    pub async fn submit(&self, input: &str) -> Result<Token, Fault> {
        if self.interaction.expires_at <= now() {
            return Err(fault("OAuth login expired; restart login"));
        }
        if !self.interaction.manual_input {
            return Err(fault(
                "this login requires its callback or device authorization",
            ));
        }
        let code = self.parse_code(input, false)?;
        self.exchange(&code).await
    }
    fn parse_code(&self, input: &str, callback: bool) -> Result<String, Fault> {
        let input = input.trim();
        if input.is_empty() || input.len() > 16384 {
            return Err(fault("authorization input is empty or too large"));
        }
        let mut returned_state = None;
        let code = if let Ok(parsed) = reqwest::Url::parse(input) {
            let pairs: BTreeMap<_, _> = parsed
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            if pairs.contains_key("error") {
                return Err(fault("OAuth authorization was denied"));
            }
            returned_state = pairs.get("state").cloned();
            pairs
                .get("code")
                .cloned()
                .ok_or_else(|| fault("authorization code is missing"))?
        } else if let Some((code, state)) = input.split_once('#') {
            returned_state = Some(state.into());
            code.into()
        } else if input.contains("code=") {
            return self.parse_code(&format!("http://localhost/?{input}"), callback);
        } else {
            input.into()
        };
        if self.settings.provider != "openrouter"
            && (returned_state
                .as_ref()
                .is_some_and(|state| state != &self.state)
                || (callback && returned_state.is_none()))
        {
            return Err(fault("OAuth state mismatch"));
        }
        if code.is_empty() {
            return Err(fault("authorization code is missing"));
        }
        Ok(code)
    }
    async fn exchange(&self, code: &str) -> Result<Token, Fault> {
        let fields = if self.settings.provider == "openrouter" {
            json!({ "code": code, "code_verifier": self.verifier, "code_challenge_method": "S256" })
        } else {
            json!({
                "grant_type": "authorization_code",
                "client_id": self.settings.client_id,
                "code": code,
                "code_verifier": self.verifier,
                "redirect_uri": self.settings.redirect_uri,
                "state": self.state,
            })
        };
        let (status, value) = post(
            &self.settings,
            &self.settings.token_url,
            fields,
            matches!(self.settings.provider.as_str(), "anthropic" | "openrouter"),
        )
        .await?;
        successful(status)?;
        Token::parse(self.settings.clone(), value, None).await
    }
    pub async fn wait(&mut self) -> Result<Token, Fault> {
        let deadline = self.interaction.expires_at.saturating_sub(now());
        tokio::time::timeout(Duration::from_secs(deadline), async {
            if self.method == "device" {
                return self.poll().await;
            }
            let listener = self
                .listener
                .as_ref()
                .ok_or_else(|| fault("callback listener is closed"))?;
            loop {
                let (mut stream, _) = listener
                    .accept()
                    .await
                    .map_err(|_| fault("OAuth callback failed"))?;
                let mut bytes = Vec::new();
                let request = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let byte = stream
                            .read_u8()
                            .await
                            .map_err(|_| fault("OAuth callback interrupted"))?;
                        bytes.push(byte);
                        if bytes.ends_with(b"\r\n\r\n") {
                            break;
                        }
                        if bytes.len() > 16384 {
                            return Err(fault("OAuth callback too large"));
                        }
                    }
                    Ok::<_, Fault>(())
                })
                .await;
                if !matches!(request, Ok(Ok(()))) {
                    continue;
                }
                let request = String::from_utf8_lossy(&bytes);
                let mut parts = request.split_whitespace();
                let method = parts.next();
                let path = parts.next().unwrap_or_default();
                let incoming = format!("http://localhost{path}");
                let matches_path = reqwest::Url::parse(&incoming)
                    .ok()
                    .zip(reqwest::Url::parse(&self.settings.redirect_uri).ok())
                    .is_some_and(|(a, b)| a.path() == b.path());
                let parsed = if method == Some("GET") && matches_path {
                    self.parse_code(&incoming, true)
                } else {
                    Err(fault("invalid callback route"))
                };
                let status = if parsed.is_ok() {
                    "200 OK"
                } else {
                    "400 Bad Request"
                };
                let answer = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: 26\r\nConnection: \
                     close\r\nCache-Control: no-store\r\n\r\nReturn to the application.\n"
                );
                let _ = stream.write_all(answer.as_bytes()).await;
                let _ = stream.shutdown().await;
                if let Ok(code) = parsed {
                    return self.exchange(&code).await;
                }
                if matches_path && request.contains("error=") {
                    return Err(fault("OAuth authorization was denied"));
                }
            }
        })
        .await
        .map_err(|_| fault("OAuth login expired; restart login"))?
    }
    async fn poll(&mut self) -> Result<Token, Fault> {
        let device = self
            .device
            .as_ref()
            .ok_or_else(|| fault("device login is missing"))?;
        let mut interval = device
            .get("interval")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(5)
            .max(1);
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            let codex = self.settings.provider == "openai-codex";
            let fields = if codex {
                json!({
                    "device_auth_id": device["device_auth_id"],
                    "user_code": device["user_code"],
                })
            } else {
                json!({
                    "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                    "client_id": self.settings.client_id,
                    "device_code": device["device_code"],
                })
            };
            let endpoint = if codex {
                &self.settings.device_token_url
            } else {
                &self.settings.token_url
            };
            let (status, value) = post(&self.settings, endpoint, fields, codex).await?;
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .or_else(|| value.pointer("/error/code").and_then(Value::as_str));
            if matches!(
                error,
                Some("authorization_pending" | "deviceauth_authorization_pending")
            ) || (codex && matches!(status, 403 | 404))
            {
                continue;
            }
            if error == Some("slow_down") {
                interval =
                    (interval + 5).max(value.get("interval").and_then(Value::as_u64).unwrap_or(0));
                continue;
            }
            if error.is_some() {
                return Err(fault(
                    "device authorization denied, expired, or rejected; restart login",
                ));
            }
            successful(status)?;
            if codex {
                let code = required(&value, "authorization_code")?;
                self.verifier = required(&value, "code_verifier")?;
                self.settings.redirect_uri = "https://auth.openai.com/deviceauth/callback".into();
                return self.exchange(&code).await;
            }
            return Token::parse(self.settings.clone(), value, None).await;
        }
    }
}
impl Token {
    async fn parse(
        settings: Settings,
        value: Value,
        old_refresh: Option<&str>,
    ) -> Result<Self, Fault> {
        if settings.provider == "github-copilot" {
            return Self::copilot(settings, required(&value, "access_token")?).await;
        }
        let access = required(
            &value,
            if settings.provider == "openrouter" {
                "key"
            } else {
                "access_token"
            },
        )?;
        let refresh = if settings.provider == "openrouter" {
            String::new()
        } else {
            value
                .get("refresh_token")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or(old_refresh)
                .ok_or_else(|| fault("OAuth refresh token is missing"))?
                .to_owned()
        };
        let expires_at = if settings.provider == "openrouter" {
            u64::MAX
        } else {
            let lifetime = value
                .get("expires_in")
                .and_then(Value::as_u64)
                .or_else(|| (settings.provider == "xai").then_some(3600))
                .filter(|v| *v > 0)
                .ok_or_else(|| fault("OAuth token expiry is invalid"))?;
            now().saturating_add(lifetime.saturating_sub(if settings.provider == "radius" {
                60.min(lifetime / 2)
            } else {
                300.min(lifetime / 2)
            }))
        };
        let account_id = if settings.provider == "openai-codex" {
            let payload = access
                .split('.')
                .nth(1)
                .and_then(|part| URL_SAFE_NO_PAD.decode(part).ok())
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .ok_or_else(|| fault("Codex token has no account claims"))?;
            Some(
                payload
                    .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| fault("Codex account ID is missing"))?
                    .to_owned(),
            )
        } else {
            None
        };
        Ok(Self {
            settings,
            access,
            refresh,
            expires_at,
            base_url: None,
            account_id,
            catalog_scope: random()?,
            available_model_ids: None,
        })
    }
    async fn copilot(settings: Settings, github_token: String) -> Result<Self, Fault> {
        let mut request = client()?
            .get(url(&settings.copilot_token_url)?)
            .bearer_auth(&github_token)
            .header("accept", "application/json")
            .header("user-agent", "eden-agent");
        for (name, value) in &settings.headers {
            request = request.header(name, value);
        }
        let (status, value) = response(request).await?;
        successful(status)?;
        let access = required(&value, "token")?;
        let expires_at = value
            .get("expires_at")
            .and_then(Value::as_u64)
            .filter(|v| *v > now())
            .ok_or_else(|| fault("Copilot token expiry is invalid"))?
            .saturating_sub(300);
        let base_url = value
            .pointer("/endpoints/api")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                access.split(';').find_map(|field| {
                    field
                        .strip_prefix("proxy-ep=")
                        .map(|s| format!("https://{}", s.replace("proxy.", "api.")))
                })
            })
            .unwrap_or_else(|| {
                if settings.domain == "github.com" {
                    "https://api.individual.githubcopilot.com".into()
                } else {
                    format!("https://copilot-api.{}", settings.domain)
                }
            });
        url(&base_url)?;
        let available_model_ids = Some(
            crate::subscription::copilot_account_models(&access, &base_url, &settings.headers)
                .await?,
        );
        Ok(Self {
            settings,
            access,
            refresh: github_token,
            expires_at,
            base_url: Some(base_url),
            account_id: None,
            catalog_scope: random()?,
            available_model_ids,
        })
    }
    pub async fn refresh(&self) -> Result<Self, Fault> {
        if self.settings.provider == "openrouter" {
            return Ok(self.clone());
        }
        if self.settings.provider == "github-copilot" {
            let mut next = Self::copilot(self.settings.clone(), self.refresh.clone()).await?;
            next.catalog_scope = self.catalog_scope.clone();
            return Ok(next);
        }
        let (status, value) = post(
            &self.settings,
            &self.settings.token_url,
            json!({
                "grant_type": "refresh_token",
                "client_id": self.settings.client_id,
                "refresh_token": self.refresh,
            }),
            self.settings.provider == "anthropic",
        )
        .await?;
        successful(status)?;
        let mut next = Self::parse(self.settings.clone(), value, Some(&self.refresh)).await?;
        next.catalog_scope = self.catalog_scope.clone();
        Ok(next)
    }
    pub fn same_account(&self, other: &Self) -> bool {
        self.catalog_scope == other.catalog_scope
    }
    pub fn credential(&self) -> CredentialReply {
        let mut headers = self.settings.headers.clone();
        if let Some(id) = &self.account_id {
            headers.insert("ChatGPT-Account-Id".into(), id.clone());
        }
        if matches!(self.settings.provider.as_str(), "kimi-coding" | "anthropic") {
            headers.insert("Authorization".into(), format!("Bearer {}", self.access));
        }
        if self.settings.provider == "anthropic" {
            headers.insert("anthropic-beta".into(), "oauth-2025-04-20".into());
        }
        CredentialReply {
            api_key: Some(self.access.clone()),
            headers,
            source: "stored_oauth".into(),
            base_url: self.base_url.clone(),
            available_model_ids: self.available_model_ids.clone(),
            catalog_scope: Some(self.catalog_scope.clone()),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn client_admission_is_required_without_copying_another_app_identity() {
        for provider in [
            "anthropic",
            "openai-codex",
            "github-copilot",
            "xai",
            "kimi-coding",
            "radius",
        ] {
            assert!(Settings::new(provider, &Config::default()).is_err());
        }
        assert!(Settings::new("openrouter", &Config::default()).is_ok());
    }
    #[test]
    fn non_loopback_plaintext_endpoints_are_rejected() {
        assert!(url("http://example.com/token").is_err());
        assert!(url("http://127.0.0.1:1234/token").is_ok());
    }
    #[tokio::test]
    async fn manual_wrong_state_is_rejected_and_listener_is_released() {
        let config = Config {
            client_id: Some("test-client".into()),
            redirect_uri: Some("http://127.0.0.1:0/callback".into()),
            ..Default::default()
        };
        let flow = Flow::start("anthropic", Some("browser"), &config)
            .await
            .unwrap();
        let port = flow.listener.as_ref().unwrap().local_addr().unwrap().port();
        assert!(
            flow.submit("http://localhost/callback?code=secret&state=wrong")
                .await
                .is_err()
        );
        drop(flow);
        assert!(TcpListener::bind(("127.0.0.1", port)).await.is_ok());
    }
}

#[cfg(test)]
#[path = "oauth_flow_tests.rs"]
mod flow_tests;
