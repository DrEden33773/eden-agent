//! Cloud identities are resolved for each request and never enter persisted targets.
use aws_sdk_bedrockruntime::config::{Credentials, ProvideCredentials, Region, retry::RetryConfig};
use eden_protocol::{Fault, models::CredentialReply};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf};
pub(crate) const AWS_PRIVATE_HEADER: &str = "x-eden-private-aws";
fn fault(message: &str) -> Fault {
    Fault::new("CredentialFailure", "model-access", message)
}
fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}
fn options<'a>(provider: &str, config: &'a Value) -> &'a Value {
    &config["credentials"]["providers"][provider]["cloud"]
}
fn adc_file() -> Option<PathBuf> {
    env("GOOGLE_APPLICATION_CREDENTIALS")
        .map(PathBuf::from)
        .or_else(|| {
            let base = {
                if cfg!(windows) {
                    env("APPDATA").map(|p| PathBuf::from(p).join("gcloud"))
                } else {
                    env("HOME").map(|p| PathBuf::from(p).join(".config/gcloud"))
                }
            }?;
            let path = base.join("application_default_credentials.json");
            path.is_file().then_some(path)
        })
}
pub(crate) fn configured(provider: &str, config: &Value) -> bool {
    let opts = options(provider, config);
    if opts["enabled"].as_bool() == Some(false) {
        return false;
    }
    match provider {
        "amazon-bedrock" => {
            opts.is_object()
                || [
                    "AWS_PROFILE",
                    "AWS_CONFIG_FILE",
                    "AWS_SHARED_CREDENTIALS_FILE",
                    "AWS_ACCESS_KEY_ID",
                    "AWS_WEB_IDENTITY_TOKEN_FILE",
                    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
                    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
                ]
                .iter()
                .any(|n| env(n).is_some())
                || env("HOME").is_some_and(|p| PathBuf::from(p).join(".aws/credentials").is_file())
        }
        "google-vertex" => opts.is_object() || adc_file().is_some(),
        _ => false,
    }
}
pub(crate) async fn resolve(
    provider: &str,
    purpose: &str,
    config: &Value,
    commands_trusted: bool,
) -> Result<Option<CredentialReply>, Fault> {
    if !configured(provider, config) {
        return Ok(None);
    }
    if purpose == "catalog" {
        return Ok(Some(CredentialReply {
            api_key: None,
            headers: BTreeMap::new(),
            source: "cloud_configured".into(),
        }));
    }
    let opts = options(provider, config);
    match provider {
        "amazon-bedrock" => {
            // An empty provider config has no retry sleep implementation. ECS otherwise
            // installs its own default retry policy independently of the STS policy.
            let region = configured_region(config).await.map(Region::new);
            let provider_config = aws_config::provider_config::ProviderConfig::empty()
                .with_region(region.clone())
                .with_retry_config(RetryConfig::standard().with_max_attempts(1))
                .with_behavior_version(Some(aws_config::BehaviorVersion::latest()));
            let imds = aws_config::imds::Client::builder()
                .configure(&provider_config)
                .max_attempts(1)
                .build();
            let builder =
                aws_config::default_provider::credentials::DefaultCredentialsChain::builder()
                    .configure(provider_config)
                    .region(region)
                    .imds_client(imds);
            // credential_process is disabled at the dependency feature boundary.
            let credentials = if let Some(profile) = opts["profile"].as_str() {
                aws_config::profile::ProfileFileCredentialsProvider::builder()
                    .configure(
                        &aws_config::provider_config::ProviderConfig::empty()
                            .with_region(configured_region(config).await.map(Region::new))
                            .with_retry_config(RetryConfig::standard().with_max_attempts(1))
                            .with_behavior_version(Some(aws_config::BehaviorVersion::latest())),
                    )
                    .profile_name(profile)
                    .build()
                    .provide_credentials()
                    .await
            } else {
                builder.build().await.provide_credentials().await
            }
            .map_err(|_| fault("AWS credential chain failed"))?;
            Ok(Some(aws_reply(&credentials)))
        }
        "google-vertex" => google_isolated(opts.clone(), commands_trusted).await,

        _ => Ok(None),
    }
}
// The official Google cache spawns refresh tasks. Owning their whole runtime
// makes cancellation and dynamic-library unload wait for those tasks to settle.
struct GoogleWorker {
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for GoogleWorker {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
async fn google_isolated(
    opts: Value,
    commands_trusted: bool,
) -> Result<Option<CredentialReply>, Fault> {
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let thread = std::thread::Builder::new()
        .name("eden-google-auth".into())
        .spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(async move {
                    tokio::select! {
                        biased;
                        _ = cancelled => Err(fault("Google credential resolution cancelled")),
                        result = google_headers(opts, commands_trusted) => result,
                    }
                }),
                Err(_) => Err(fault("cannot initialize Google credential runtime")),
            };
            let _ = sender.send(result);
        })
        .map_err(|_| fault("cannot initialize Google credential worker"))?;
    let _worker = GoogleWorker {
        cancel: Some(cancel),
        thread: Some(thread),
    };
    receiver
        .await
        .map_err(|_| fault("Google credential worker failed"))?
}
async fn google_headers(
    opts: Value,
    commands_trusted: bool,
) -> Result<Option<CredentialReply>, Fault> {
    let file = opts["service_account_file"]
        .as_str()
        .or_else(|| opts["adc_file"].as_str())
        .map(PathBuf::from)
        .or_else(adc_file);
    let quota = env("GOOGLE_CLOUD_QUOTA_PROJECT")
        .or_else(|| opts["quota_project"].as_str().map(str::to_owned));
    let credentials = if let Some(path) = file {
        let bytes = std::fs::read(path).map_err(|_| fault("cannot read Google credential file"))?;
        let json: Value =
            serde_json::from_slice(&bytes).map_err(|_| fault("invalid Google credential file"))?;
        if opts["service_account_file"].is_string() && json["type"] != "service_account" {
            return Err(fault(
                "Google service_account_file must contain service-account credentials",
            ));
        }
        if contains_executable(&json) {
            return Err(fault(if commands_trusted {
                "Google executable ADC is unsupported because its subprocess cannot be cancelled safely"
            } else {
                "Google executable credentials require command trust"
            }));
        }
        // Consume the same inspected value, avoiding a second mutable-file lookup.
        match json["type"].as_str().unwrap_or("") {
            "service_account" => {
                let mut builder =
                    google_cloud_auth::credentials::service_account::Builder::new(json);
                if let Some(project) = quota {
                    builder = builder.with_quota_project_id(project);
                }
                builder.build()
            }
            "authorized_user" => {
                let mut builder = google_cloud_auth::credentials::user_account::Builder::new(json)
                    .with_scopes(["https://www.googleapis.com/auth/cloud-platform"]);
                if let Some(project) = quota {
                    builder = builder.with_quota_project_id(project);
                }
                builder.build()
            }
            "external_account" => {
                let mut builder =
                    google_cloud_auth::credentials::external_account::Builder::new(json)
                        .with_scopes(["https://www.googleapis.com/auth/cloud-platform"]);
                if let Some(project) = quota {
                    builder = builder.with_quota_project_id(project);
                }
                builder.build()
            }
            "impersonated_service_account" => {
                let mut builder = google_cloud_auth::credentials::impersonated::Builder::new(json)
                    .with_scopes(["https://www.googleapis.com/auth/cloud-platform"]);
                if let Some(project) = quota {
                    builder = builder.with_quota_project_id(project);
                }
                builder.build()
            }
            _ => return Err(fault("unsupported Google credential type")),
        }
        .map_err(|_| fault("invalid Google credentials"))?
    } else {
        let mut builder = google_cloud_auth::credentials::mds::Builder::default()
            .with_scopes(["https://www.googleapis.com/auth/cloud-platform"]);
        if let Some(project) = quota {
            builder = builder.with_quota_project_id(project);
        }
        builder
            .build()
            .map_err(|_| fault("Google ADC discovery failed"))?
    };
    let result = credentials
        .headers(Default::default())
        .await
        .map_err(|_| fault("Google credential resolution failed"))?;
    let google_cloud_auth::credentials::CacheableResource::New { data, .. } = result else {
        return Err(fault("Google credentials returned no headers"));
    };
    let headers = data
        .iter()
        .map(|(k, v)| {
            Ok((
                k.as_str().to_owned(),
                v.to_str()
                    .map_err(|_| fault("invalid Google credential header"))?
                    .to_owned(),
            ))
        })
        .collect::<Result<_, Fault>>()?;
    Ok(Some(CredentialReply {
        api_key: None,
        headers,
        source: "google_adc".into(),
    }))
}
fn contains_executable(value: &Value) -> bool {
    match value {
        Value::Object(o) => o.contains_key("executable") || o.values().any(contains_executable),
        Value::Array(a) => a.iter().any(contains_executable),
        _ => false,
    }
}
fn aws_reply(credentials: &Credentials) -> CredentialReply {
    let value = serde_json::json!({
        "access_key": credentials.access_key_id(),
        "secret_key": credentials.secret_access_key(),
        "session_token": credentials.session_token(),
    });
    CredentialReply {
        api_key: None,
        headers: BTreeMap::from([(AWS_PRIVATE_HEADER.into(), value.to_string())]),
        source: "aws_chain".into(),
    }
}
pub(crate) fn aws_credentials(reply: &CredentialReply) -> Result<Credentials, Fault> {
    let value: Value = serde_json::from_str(
        reply
            .headers
            .get(AWS_PRIVATE_HEADER)
            .ok_or_else(|| fault("AWS credentials missing"))?,
    )
    .map_err(|_| fault("invalid private AWS credential envelope"))?;
    Ok(Credentials::new(
        value["access_key"]
            .as_str()
            .ok_or_else(|| fault("AWS access key missing"))?,
        value["secret_key"]
            .as_str()
            .ok_or_else(|| fault("AWS secret key missing"))?,
        value["session_token"].as_str().map(str::to_owned),
        None,
        "eden-private",
    ))
}
/// Profile discovery is local-only, so catalog targets can freeze a region without metadata requests.
pub(crate) async fn configured_region(config: &Value) -> Option<String> {
    use aws_config::meta::region::ProvideRegion;
    let opts = options("amazon-bedrock", config);
    if let Some(region) = opts["region"].as_str().filter(|s| !s.is_empty()) {
        return Some(region.into());
    }
    if let Some(region) = env("AWS_REGION").or_else(|| env("AWS_DEFAULT_REGION")) {
        return Some(region);
    }
    let mut provider = aws_config::profile::ProfileFileRegionProvider::builder();
    if let Some(profile) = opts["profile"].as_str() {
        provider = provider.profile_name(profile);
    }
    provider
        .build()
        .region()
        .await
        .map(|region| region.as_ref().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn catalog_explicit_configuration_never_reads_key_file() {
        let config = serde_json::json!({
            "credentials": {
                "providers": {
                    "google-vertex": {
                        "cloud": { "service_account_file": "/missing/private-key.json" },
                    },
                },
            },
        });
        let reply = resolve("google-vertex", "catalog", &config, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.source, "cloud_configured");
        assert!(reply.headers.is_empty());
        assert!(reply.api_key.is_none());
    }
    #[tokio::test]
    async fn unrelated_provider_has_no_cloud_fallback() {
        assert!(
            resolve("openai", "inference", &Value::Null, false)
                .await
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn official_chain_profiles_and_environment_precedence_are_isolated() {
        let directory =
            std::env::temp_dir().join(format!("eden-cloud-profile-{}", std::process::id()));
        std::fs::create_dir_all(directory.join(".aws")).unwrap();
        std::fs::write(directory.join(".aws/credentials"), "[fixture]\naws_access_key_id = fixture-access\naws_secret_access_key = fixture-secret\naws_session_token = fixture-session\n").unwrap();
        std::fs::write(
            directory.join(".aws/config"),
            "[profile fixture]\nregion = eu-west-1\n",
        )
        .unwrap();
        for environment in [false, true] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .env_clear()
                .env("HOME", &directory)
                .env("USERPROFILE", &directory)
                .env("AWS_CONFIG_FILE", directory.join(".aws/config"))
                .env(
                    "AWS_SHARED_CREDENTIALS_FILE",
                    directory.join(".aws/credentials"),
                )
                .env("EDEN_CLOUD_CHAIN_TEST", "1")
                .args([
                    "--exact",
                    "cloud::tests::isolated_chain_child",
                    "--nocapture",
                ]);
            if environment {
                child
                    .env("AWS_ACCESS_KEY_ID", "env-access")
                    .env("AWS_SECRET_ACCESS_KEY", "env-secret")
                    .env("AWS_REGION", "us-west-2");
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[tokio::test]
    async fn official_assume_role_and_web_identity_use_controlled_sts() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        for web in [false, true] {
            let directory =
                std::env::temp_dir().join(format!("eden-cloud-sts-{}-{web}", std::process::id()));
            std::fs::create_dir_all(&directory).unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let config = if web {
                "[profile fixture]\nregion = eu-west-1\n"
            } else {
                "[profile fixture]\nregion = eu-west-1\nrole_arn = arn:aws:iam::123456789012:role/fixture\nsource_profile = base\n"
            };
            std::fs::write(directory.join("config"), config).unwrap();
            std::fs::write(directory.join("credentials"),"[base]\naws_access_key_id = source-access\naws_secret_access_key = source-secret\n").unwrap();
            std::fs::write(directory.join("token"), "fixture-web-token").unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                        let len: usize = headers
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:").map(str::trim))
                            .unwrap()
                            .parse()
                            .unwrap();
                        if bytes.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let action = if web {
                    "AssumeRoleWithWebIdentity"
                } else {
                    "AssumeRole"
                };
                let body = format!(
                    "<{action}Response xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><{action}Result><Credentials><AccessKeyId>fixture-access</AccessKeyId><SecretAccessKey>fixture-secret</SecretAccessKey><SessionToken>fixture-session</SessionToken><Expiration>2099-01-01T00:00:00Z</Expiration></Credentials><AssumedRoleUser><AssumedRoleId>id:fixture</AssumedRoleId><Arn>arn:aws:sts::123456789012:assumed-role/fixture/session</Arn></AssumedRoleUser></{action}Result><ResponseMetadata><RequestId>fixture</RequestId></ResponseMetadata></{action}Response>"
                );
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                String::from_utf8(bytes).unwrap()
            });
            let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
            child
                .kill_on_drop(true)
                .env_clear()
                .env("HOME", &directory)
                .env("USERPROFILE", &directory)
                .env("AWS_CONFIG_FILE", directory.join("config"))
                .env("AWS_SHARED_CREDENTIALS_FILE", directory.join("credentials"))
                .env("AWS_ENDPOINT_URL_STS", endpoint)
                .env("EDEN_CLOUD_CHAIN_TEST", "1")
                .args([
                    "--exact",
                    "cloud::tests::isolated_chain_child",
                    "--nocapture",
                ]);
            if web {
                child
                    .env("AWS_PROFILE", "fixture")
                    .env("EDEN_WEB_IDENTITY_TEST", "1")
                    .env("AWS_WEB_IDENTITY_TOKEN_FILE", directory.join("token"))
                    .env("AWS_ROLE_ARN", "arn:aws:iam::123456789012:role/fixture");
            }
            let output = tokio::time::timeout(std::time::Duration::from_secs(10), child.output())
                .await
                .unwrap()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let wire = server.await.unwrap();
            if web {
                assert!(wire.contains("fixture-web-token"));
                assert!(wire.contains("Action=AssumeRoleWithWebIdentity"));
            } else {
                assert!(wire.contains("AWS4-HMAC-SHA256"));
                assert!(wire.contains("Action=AssumeRole"));
            }
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
    #[tokio::test]
    async fn google_adc_headers_and_cancellation_settle_refresh_worker() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
            sync::oneshot,
        };
        for cancel in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let path = std::env::temp_dir().join(format!(
                "eden-google-auth-{}-{cancel}.json",
                std::process::id()
            ));
            std::fs::write(
                &path,
                serde_json::json!({
                    "type": "authorized_user",
                    "client_id": "fixture-client",
                    "client_secret": "fixture-secret",
                    "refresh_token": "fixture-refresh",
                    "token_uri":
                        format!("http://{}/token", listener.local_addr().unwrap()),
                })
                .to_string(),
            )
            .unwrap();
            let (seen, ready) = oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                        let len: usize = headers
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:").map(str::trim))
                            .unwrap()
                            .parse()
                            .unwrap();
                        if bytes.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                seen.send(()).unwrap();
                if !cancel {
                    let body = r#"{"access_token":"fixture-google-token","token_type":"Bearer","expires_in":3600}"#;
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                }
                socket.read(&mut chunk).await.unwrap()
            });
            let options = serde_json::json!({ "adc_file": path, "quota_project": "fixture-quota" });
            let task = tokio::spawn(google_isolated(options, false));
            ready.await.unwrap();
            if cancel {
                task.abort();
                assert!(task.await.err().unwrap().is_cancelled());
            } else {
                let reply = task.await.unwrap().unwrap().unwrap();
                assert_eq!(
                    reply.headers["authorization"],
                    "Bearer fixture-google-token"
                );
                assert_eq!(reply.headers["x-goog-user-project"], "fixture-quota");
            }
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(5), server)
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
            std::fs::remove_file(path).unwrap();
        }
    }
    #[tokio::test]
    async fn isolated_chain_child() {
        if env("EDEN_CLOUD_CHAIN_TEST").is_none() {
            return;
        }
        let config = serde_json::json!({
            "credentials": {
                "providers": {
                    "amazon-bedrock": {
                        "cloud": if env("EDEN_WEB_IDENTITY_TEST").is_some() {
                                serde_json::json!({ "enabled": true })
                            } else {
                                serde_json::json!({ "profile": "fixture" })
                            },
                    },
                },
            },
        });
        let region = configured_region(&config).await.unwrap();
        let reply = resolve("amazon-bedrock", "inference", &config, false)
            .await
            .unwrap()
            .unwrap();
        let credentials = aws_credentials(&reply).unwrap();
        if env("AWS_ACCESS_KEY_ID").is_some() {
            assert_eq!(credentials.access_key_id(), "fixture-access");
            assert_eq!(region, "us-west-2");
        } else {
            assert_eq!(credentials.access_key_id(), "fixture-access");
            assert_eq!(credentials.session_token(), Some("fixture-session"));
            assert_eq!(region, "eu-west-1");
        }
    }
    #[test]
    fn executable_source_is_found_inside_impersonation_configuration() {
        assert!(contains_executable(&serde_json::json!({
            "source_credentials": {
                "credential_source": { "executable": { "command": "unsafe" } },
            },
        })));
        assert!(!contains_executable(&serde_json::json!({
            "credential_source": { "file": "token" },
        })));
    }
    #[test]
    fn private_identity_roundtrips_without_debug_or_public_target() {
        let credentials = aws_sdk_bedrockruntime::config::Credentials::new(
            "access",
            "secret",
            Some("session".into()),
            None,
            "test",
        );
        let reply = aws_reply(&credentials);
        let decoded = aws_credentials(&reply).unwrap();
        assert_eq!(decoded.access_key_id(), "access");
        assert_eq!(decoded.session_token(), Some("session"));
        assert!(reply.api_key.is_none());
    }
}
