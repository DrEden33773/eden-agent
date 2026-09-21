//! Secret material stays inside private role calls and permission-restricted storage.
use eden_plugin_sdk::{CallContext, Package};
use eden_protocol::{Fault, models::*};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::File,
    io::Write,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::Mutex;
static NEXT: AtomicU64 = AtomicU64::new(1);
#[derive(Default, Deserialize)]
#[serde(default)]
struct ProviderConfig {
    env: Option<String>,
    literal: Option<String>,
    command: Option<String>,
    headers: BTreeMap<String, String>,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct Config {
    path: Option<PathBuf>,
    providers: BTreeMap<String, ProviderConfig>,
}
#[derive(Default, Serialize, Deserialize)]
struct Stored {
    keys: BTreeMap<String, String>,
}
struct Credentials {
    config: Config,
    commands_trusted: bool,
    cloud_config: Value,
    operations: Mutex<BTreeMap<String, AuthReply>>,
    command_cache: Mutex<BTreeMap<String, String>>,
    cwd: PathBuf,
}
fn fault(message: &str) -> Fault {
    Fault::new("CredentialFailure", "model-access", message)
}
pub(crate) fn register(package: Package, value: &Value) -> Result<Package, Fault> {
    let mut config: Config = serde_json::from_value(
        value
            .get("credentials")
            .cloned()
            .unwrap_or(serde_json::json!({})),
    )
    .map_err(|_| fault("invalid credentials configuration"))?;
    if config.path.is_none() {
        config.path = value
            .get("global_dir")
            .and_then(Value::as_str)
            .map(|p| PathBuf::from(p).join("credentials.json"));
    }
    let state = Arc::new(Credentials {
        config,
        cloud_config: value.clone(),
        commands_trusted: value
            .get("commands_trusted")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        operations: Mutex::new(BTreeMap::new()),
        command_cache: Mutex::new(BTreeMap::new()),
        cwd: value
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".")),
    });
    let resolver = state.clone();
    Ok(package
        .service(CREDENTIAL_SOURCE, move |request: CredentialRequest, cx| {
            let state = resolver.clone();
            async move { state.resolve(request, Some(&cx)).await }
        })
        .service(AUTH, move |request: AuthRequest, cx| {
            let state = state.clone();
            async move { state.auth(request, Some(&cx)).await }
        }))
}
#[cfg(not(windows))]
fn private_open(path: &std::path::Path) -> Result<File, Fault> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|_| fault("cannot open private credential storage"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file
            .metadata()
            .map_err(|_| fault("cannot inspect credential permissions"))?
            .permissions()
            .mode()
            & 0o077
            != 0
        {
            return Err(fault(
                "credential storage permissions must restrict access to its owner",
            ));
        }
    }
    Ok(file)
}
#[cfg(windows)]
fn private_open(path: &std::path::Path) -> Result<File, Fault> {
    use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree},
        Security::{
            Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
            DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, SetKernelObjectSecurity,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS,
            WRITE_DAC,
        },
    };
    let sddl: Vec<u16> = "D:P(A;;FA;;;OW)".encode_utf16().chain(Some(0)).collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: UTF-16 input remains alive; Win32 allocates the descriptor, released below.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(fault("cannot create private credential permissions"));
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: All input buffers and attributes live through CreateFileW; handle ownership transfers to File.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE | WRITE_DAC,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        // SAFETY: This descriptor was allocated above and has no remaining users.
        unsafe {
            LocalFree(descriptor);
        };
        return Err(fault("cannot open private credential storage"));
    }
    // SAFETY: The valid owned handle remains live; descriptor is valid through this call.
    let secured = unsafe { SetKernelObjectSecurity(handle, DACL_SECURITY_INFORMATION, descriptor) };
    // SAFETY: Win32 allocated this descriptor above; no references remain after this point.
    unsafe {
        LocalFree(descriptor);
    }
    // SAFETY: CreateFileW returned a new owned handle and no other owner exists.
    let file = unsafe { File::from_raw_handle(handle) };
    if secured == 0 {
        return Err(fault("cannot restrict credential storage permissions"));
    }
    Ok(file)
}
impl Credentials {
    fn store<T>(
        &self,
        change: impl FnOnce(&mut Stored) -> Result<T, Fault>,
        write: bool,
    ) -> Result<T, Fault> {
        let Some(path) = &self.config.path else {
            if write {
                return Err(fault("credential storage path is not configured"));
            }
            return change(&mut Stored::default());
        };
        let parent = path
            .parent()
            .ok_or_else(|| fault("invalid credential storage path"))?;
        std::fs::create_dir_all(parent).map_err(|_| fault("cannot create credential directory"))?;
        let lock = private_open(&path.with_extension("lock"))?;
        lock.lock()
            .map_err(|_| fault("cannot lock credential storage"))?;
        let mut stored = if path.exists() {
            let file = private_open(path)?;
            serde_json::from_reader(file).map_err(|_| fault("credential storage is invalid"))?
        } else {
            Stored::default()
        };
        let result = change(&mut stored)?;
        if write {
            let temporary = path.with_extension(format!(
                "{}.{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = private_open(&temporary)?;
            let bytes = serde_json::to_vec(&stored)
                .map_err(|_| fault("cannot encode credential storage"))?;
            let written = file.write_all(&bytes).and_then(|()| file.sync_all());
            // Windows private handles deliberately exclude delete sharing. Close
            // the temporary before replacement, retaining the separate store lock.
            drop(file);
            let outcome = written.and_then(|()| std::fs::rename(&temporary, path));
            if outcome.is_err() {
                let _ = std::fs::remove_file(&temporary);
                return Err(fault("cannot atomically persist credentials"));
            }
        }
        drop(lock);
        Ok(result)
    }
    async fn resolve(
        &self,
        request: CredentialRequest,
        cx: Option<&CallContext>,
    ) -> Result<CredentialReply, Fault> {
        let mut headers = BTreeMap::new();
        if request.purpose != "catalog"
            && let Some(config) = self.config.providers.get(&request.provider)
        {
            for (name, value) in &config.headers {
                let value = if let Some(command) = value.strip_prefix('!') {
                    if !self.commands_trusted {
                        return Err(fault(
                            "credential header command requires trusted configuration",
                        ));
                    }
                    self.command(command, cx).await?
                } else if let Some(env) = value.strip_prefix("${").and_then(|v| v.strip_suffix('}'))
                {
                    std::env::var(env)
                        .map_err(|_| fault("credential header environment variable is missing"))?
                } else {
                    value.clone()
                };
                headers.insert(name.clone(), value);
            }
        }
        let reply = |api_key, source: &str| {
            Ok(CredentialReply {
                api_key,
                headers: headers.clone(),
                source: source.into(),
            })
        };
        if let Some(key) = request.explicit {
            if key.trim().is_empty() {
                return Err(fault("explicit API key is empty"));
            }
            return reply(Some(key), "explicit");
        }
        if let Some(key) = self.store(|s| Ok(s.keys.get(&request.provider).cloned()), false)? {
            return reply(Some(key), "stored");
        }
        let configured = self.config.providers.get(&request.provider);
        let default_env = match request.provider.as_str() {
            "openai" => "OPENAI_API_KEY".into(),
            "anthropic" => "ANTHROPIC_API_KEY".into(),
            "deepseek" => "DEEPSEEK_API_KEY".into(),
            "kimi-coding" => "KIMI_API_KEY".into(),
            "moonshotai" | "moonshotai-cn" => "MOONSHOT_API_KEY".into(),
            "opencode-go" => "OPENCODE_API_KEY".into(),
            "huggingface" => "HF_TOKEN".into(),
            "vercel-ai-gateway" => "AI_GATEWAY_API_KEY".into(),
            "qwen-token-plan-individual" => "QWEN_TOKEN_PLAN_API_KEY".into(),
            // A GitHub user token requires the subscription token exchange, not direct inference.
            "github-copilot" => "EDEN_COPILOT_INFERENCE_KEY".into(),
            "google" => "GEMINI_API_KEY".into(),
            "google-vertex" => "GOOGLE_CLOUD_API_KEY".into(),
            "amazon-bedrock" => "AWS_BEARER_TOKEN_BEDROCK".into(),
            "cloudflare-ai-gateway" | "cloudflare-workers-ai" => "CLOUDFLARE_API_KEY".into(),
            "azure-openai-responses" => "AZURE_OPENAI_API_KEY".into(),
            p => format!("{}_API_KEY", p.to_uppercase().replace('-', "_")),
        };
        let env = configured
            .and_then(|c| c.env.as_deref())
            .unwrap_or(&default_env);
        if let Ok(key) = std::env::var(env)
            && !key.trim().is_empty()
        {
            return reply(Some(key), "environment");
        }
        if let Some(c) = configured {
            if let Some(key) = &c.literal {
                return reply(Some(key.clone()), "custom");
            }
            if let Some(command) = &c.command {
                if request.purpose == "catalog" {
                    return reply(
                        None,
                        if self.commands_trusted {
                            "command_configured"
                        } else {
                            "untrusted_command"
                        },
                    );
                }
                if !self.commands_trusted {
                    return Err(fault("credential command requires trusted configuration"));
                }
                let key = self.command(command, cx).await?;
                return reply(Some(key), "command");
            }
        }
        if let Some(mut cloud) = crate::cloud::resolve(
            &request.provider,
            &request.purpose,
            &self.cloud_config,
            self.commands_trusted,
        )
        .await?
        {
            cloud.headers.extend(headers.clone());
            return Ok(cloud);
        }
        if configured.is_some_and(|c| {
            !c.headers.is_empty()
                && c.headers.values().all(|value| {
                    if value.starts_with('!') {
                        self.commands_trusted
                    } else if let Some(env) =
                        value.strip_prefix("${").and_then(|v| v.strip_suffix('}'))
                    {
                        std::env::var(env).is_ok_and(|v| !v.is_empty())
                    } else {
                        !value.is_empty()
                    }
                })
        }) {
            return reply(None, "headers_configured");
        }
        reply(None, "missing")
    }
    async fn command(&self, command: &str, cx: Option<&CallContext>) -> Result<String, Fault> {
        use tokio::io::AsyncReadExt;
        // A fresh package on reload owns a fresh cache. Environment changes invalidate
        // the cache without exposing environment values in logs or public state.
        let environment: BTreeMap<_, _> = std::env::vars_os().collect();
        let cache_key = format!("{:?}:{command}:{environment:?}", self.cwd);
        let mut cache = self.command_cache.lock().await;
        if let Some(key) = cache.get(&cache_key) {
            return Ok(key.clone());
        }
        #[cfg(unix)]
        let (shell, args) = ("sh", vec!["-c", command]);
        #[cfg(windows)]
        let (shell, args) = ("powershell", vec!["-NoProfile", "-Command", command]);
        let (mut child, tree) = eden_process::spawn(shell, &args, &self.cwd)
            .await
            .map_err(|_| fault("credential command could not start"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| fault("credential command output is unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| fault("credential command output is unavailable"))?;
        let read = async {
            let mut bytes = Vec::new();
            (&mut stdout)
                .take(65537)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| fault("credential command output failed"))?;
            tokio::io::copy(&mut stdout, &mut tokio::io::sink())
                .await
                .map_err(|_| fault("credential command output failed"))?;
            Ok::<_, Fault>(bytes)
        };
        let drain =
            async { tokio::io::copy(&mut stderr.take(65537), &mut tokio::io::sink()).await };
        let wait = async {
            let result = child.wait().await;
            tree.cleanup_descendants()
                .map_err(|_| fault("credential command cleanup failed"))?;
            result.map_err(|_| fault("credential command failed"))
        };
        let run = async {
            let (status, bytes, _) = tokio::join!(wait, read, drain);
            Ok::<_, Fault>((status?, bytes?))
        };
        let result = if let Some(cx) = cx {
            let cancel = cx.scope.cancellation();
            tokio::select! {
                _ = cancel.cancelled() => None,
                result = run => Some(result),
            }
        } else {
            Some(run.await)
        };
        let Some(result) = result else {
            tree.terminate_group()
                .map_err(|_| fault("credential command cleanup failed"))?;
            child
                .wait()
                .await
                .map_err(|_| fault("credential command cleanup failed"))?;
            tree.settle()
                .await
                .map_err(|_| fault("credential command cleanup failed"))?;
            return Err(fault("credential command cancelled"));
        };
        tree.settle()
            .await
            .map_err(|_| fault("credential command cleanup failed"))?;
        let (status, bytes) = result?;
        if !status.success() || bytes.len() > 65536 {
            return Err(fault("credential command failed or exceeded output limit"));
        }
        let key = String::from_utf8(bytes)
            .map_err(|_| fault("credential command returned invalid text"))?
            .trim()
            .to_owned();
        if key.is_empty() {
            return Err(fault("credential command returned an empty key"));
        }
        cache.insert(cache_key, key.clone());
        Ok(key)
    }
    async fn auth(
        &self,
        request: AuthRequest,
        cx: Option<&CallContext>,
    ) -> Result<AuthReply, Fault> {
        match request {
            AuthRequest::Start { provider } => {
                let id = format!("key-{}", NEXT.fetch_add(1, Ordering::Relaxed));
                let reply = AuthReply {
                    operation_id: Some(id.clone()),
                    provider,
                    status: "awaiting_input".into(),
                    challenge: Some("api_key".into()),
                    source: None,
                };
                self.operations.lock().await.insert(id, reply.clone());
                Ok(reply)
            }
            AuthRequest::Input {
                operation_id,
                api_key,
            } => {
                if api_key.trim().is_empty() {
                    return Err(fault("API key is empty"));
                }
                let mut operations = self.operations.lock().await;
                let op = operations
                    .get_mut(&operation_id)
                    .ok_or_else(|| fault("authentication operation is unknown"))?;
                if op.status != "awaiting_input" {
                    return Err(fault("authentication operation is not awaiting input"));
                }
                self.store(
                    |s| {
                        s.keys.insert(op.provider.clone(), api_key);
                        Ok(())
                    },
                    true,
                )?;
                op.status = "completed".into();
                op.challenge = None;
                op.source = Some("stored".into());
                Ok(op.clone())
            }
            AuthRequest::Cancel { operation_id } => {
                let mut operations = self.operations.lock().await;
                let op = operations
                    .get_mut(&operation_id)
                    .ok_or_else(|| fault("authentication operation is unknown"))?;
                if op.status == "awaiting_input" {
                    op.status = "cancelled".into();
                    op.challenge = None;
                }
                Ok(op.clone())
            }
            AuthRequest::Status { operation_id } => self
                .operations
                .lock()
                .await
                .get(&operation_id)
                .cloned()
                .ok_or_else(|| fault("authentication operation is unknown")),
            AuthRequest::Logout { provider } => {
                let mut operations = self.operations.lock().await;
                for operation in operations
                    .values_mut()
                    .filter(|o| o.provider == provider && o.status == "awaiting_input")
                {
                    operation.status = "cancelled".into();
                    operation.challenge = None;
                }
                self.store(
                    |s| {
                        s.keys.remove(&provider);
                        Ok(())
                    },
                    true,
                )?;
                let resolved = self
                    .resolve(
                        CredentialRequest {
                            provider: provider.clone(),
                            explicit: None,
                            purpose: "status".into(),
                        },
                        cx,
                    )
                    .await?;
                Ok(AuthReply {
                    operation_id: None,
                    provider,
                    status: if resolved.api_key.is_some() || !resolved.headers.is_empty() {
                        "external_credentials_remain"
                    } else {
                        "logged_out"
                    }
                    .into(),
                    challenge: None,
                    source: Some(resolved.source),
                })
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn credentials() -> Credentials {
        Credentials {
            cloud_config: serde_json::json!({}),
            config: Config {
                path: Some(
                    std::env::temp_dir()
                        .join(format!(
                            "eden-key-test-{}-{}",
                            std::process::id(),
                            NEXT.fetch_add(1, Ordering::Relaxed)
                        ))
                        .join("keys.json"),
                ),
                ..Default::default()
            },
            commands_trusted: false,
            operations: Mutex::new(BTreeMap::new()),
            command_cache: Mutex::new(BTreeMap::new()),
            cwd: PathBuf::from("."),
        }
    }
    #[test]
    fn credential_writer_child() {
        let Ok(path) = std::env::var("EDEN_TEST_CREDENTIAL_CHILD_PATH") else {
            return;
        };
        let mut state = credentials();
        state.config.path = Some(PathBuf::from(path));
        let provider = std::env::var("EDEN_TEST_CREDENTIAL_CHILD_PROVIDER").unwrap();
        writeln!(std::io::stdout(), "READY").unwrap();
        std::io::stdout().flush().unwrap();
        state
            .store(
                |s| {
                    s.keys.insert(provider, "child-key".into());
                    Ok(())
                },
                true,
            )
            .unwrap();
    }
    #[test]
    fn concurrent_process_writes_preserve_both_providers() {
        use std::io::{BufRead, BufReader};
        let state = credentials();
        state.store(|_| Ok(()), true).unwrap();
        let path = state.config.path.as_ref().unwrap();
        let lock = private_open(&path.with_extension("lock")).unwrap();
        lock.lock().unwrap();
        let mut children = Vec::new();
        for provider in ["one", "two"] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "credentials::tests::credential_writer_child",
                    "--nocapture",
                ])
                .env("EDEN_TEST_CREDENTIAL_CHILD_PATH", path)
                .env("EDEN_TEST_CREDENTIAL_CHILD_PROVIDER", provider)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut reader = BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            loop {
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line.contains("READY") {
                    break;
                }
                line.clear();
            }
            children.push((child, reader));
        }
        drop(lock);
        for (mut child, mut output) in children {
            std::io::copy(&mut output, &mut std::io::sink()).unwrap();
            assert!(child.wait().unwrap().success());
        }
        assert_eq!(state.store(|s| Ok(s.keys.len()), false).unwrap(), 2);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
    #[tokio::test]
    async fn private_header_only_catalog_check_never_runs_command() {
        let mut state = credentials();
        state.commands_trusted = true;
        state.config.providers.insert(
            "header-only".into(),
            ProviderConfig {
                headers: BTreeMap::from([("Authorization".into(), "!exit 42".into())]),
                ..Default::default()
            },
        );
        let status = state
            .resolve(
                CredentialRequest {
                    provider: "header-only".into(),
                    explicit: None,
                    purpose: "catalog".into(),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(status.source, "headers_configured");
        assert!(status.headers.is_empty());
        assert!(status.api_key.is_none());
        std::fs::remove_dir_all(state.config.path.unwrap().parent().unwrap()).unwrap();
    }
    #[tokio::test]
    async fn logout_reports_remaining_private_header_authentication() {
        let mut state = credentials();
        state.config.providers.insert(
            "header-only".into(),
            ProviderConfig {
                headers: BTreeMap::from([("Authorization".into(), "Bearer secret-canary".into())]),
                ..Default::default()
            },
        );
        let reply = state
            .auth(
                AuthRequest::Logout {
                    provider: "header-only".into(),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(reply.status, "external_credentials_remain");
        assert!(
            !serde_json::to_string(&reply)
                .unwrap()
                .contains("secret-canary")
        );
        std::fs::remove_dir_all(state.config.path.unwrap().parent().unwrap()).unwrap();
    }
    #[tokio::test]
    async fn logout_cancels_pending_input_and_status_never_contains_secret() {
        let state = credentials();
        let started = state
            .auth(
                AuthRequest::Start {
                    provider: "one".into(),
                },
                None,
            )
            .await
            .unwrap();
        let id = started.operation_id.unwrap();
        state
            .auth(
                AuthRequest::Logout {
                    provider: "one".into(),
                },
                None,
            )
            .await
            .unwrap();
        assert!(
            state
                .auth(
                    AuthRequest::Input {
                        operation_id: id.clone(),
                        api_key: "canary-secret".into()
                    },
                    None
                )
                .await
                .is_err()
        );
        let status = state
            .auth(AuthRequest::Status { operation_id: id }, None)
            .await
            .unwrap();
        assert_eq!(status.status, "cancelled");
        assert!(!serde_json::to_string(&status).unwrap().contains("canary"));
        std::fs::remove_dir_all(state.config.path.unwrap().parent().unwrap()).unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn trusted_command_is_cached_and_reaps_background_children() {
        let mut state = credentials();
        state.commands_trusted = true;
        let dir = state.config.path.as_ref().unwrap().parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        let marker = dir.join("calls");
        let command = format!("printf x >> '{}'; sleep 100 & printf key", marker.display());
        assert_eq!(state.command(&command, None).await.unwrap(), "key");
        assert_eq!(state.command(&command, None).await.unwrap(), "key");
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "x");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn insecure_existing_file_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let state = credentials();
        let path = state.config.path.as_ref().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "{}").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(state.store(|_| Ok(()), false).is_err());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
    #[tokio::test]
    async fn explicit_overrides_stored_and_storage_retains_other_providers() {
        let state = credentials();
        state
            .store(
                |s| {
                    s.keys.insert("one".into(), "stored".into());
                    s.keys.insert("two".into(), "other".into());
                    Ok(())
                },
                true,
            )
            .unwrap();
        let result = state
            .resolve(
                CredentialRequest {
                    provider: "one".into(),
                    explicit: Some("explicit".into()),
                    purpose: String::new(),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.api_key.as_deref(), Some("explicit"));
        state
            .store(
                |s| {
                    s.keys.remove("one");
                    Ok(())
                },
                true,
            )
            .unwrap();
        assert_eq!(
            state
                .store(|s| Ok(s.keys.get("two").cloned()), false)
                .unwrap()
                .as_deref(),
            Some("other")
        );
        std::fs::remove_dir_all(state.config.path.unwrap().parent().unwrap()).unwrap();
    }
    #[tokio::test]
    async fn untrusted_command_is_rejected_before_execution() {
        let mut state = credentials();
        state.config.providers.insert(
            "one".into(),
            ProviderConfig {
                command: Some("printf secret".into()),
                ..Default::default()
            },
        );
        let err = state
            .resolve(
                CredentialRequest {
                    provider: "one".into(),
                    explicit: None,
                    purpose: String::new(),
                },
                None,
            )
            .await
            .err()
            .unwrap();
        assert!(err.message.contains("trusted"));
        std::fs::remove_dir_all(state.config.path.unwrap().parent().unwrap()).unwrap();
    }
}
