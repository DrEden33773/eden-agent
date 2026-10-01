//! Shared local authority for explicit Session creation, discovery and opening.
//! Published writers own accepted work independently of any frontend process.
use eden_agent::{SavedSession, Session};
use eden_protocol::Fault;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Launch settings belong to the installed CLI, independent of the selected Session.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Lifecycle {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub cwd: PathBuf,
    pub state_dir: PathBuf,
    pub legacy_directories: Vec<PathBuf>,
    #[serde(skip)]
    startup_cleanup: std::sync::Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}
/// Keep the history locator and host instance separate from persistent Session identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Opened {
    pub endpoint: PathBuf,
    pub history: Option<PathBuf>,
    pub session_id: u64,
    pub instance: String,
    pub outcome: OpenOutcome,
    pub cleanup: Cleanup,
    pub read_only: bool,
    #[serde(default)]
    pub draft: bool,
    pub diagnostic: Option<String>,
}
impl Opened {
    /// A view uses the canonical locator; duplicate Session IDs in copied files remain distinct targets.
    pub fn view_key(&self) -> String {
        self.history.as_ref().map_or_else(
            || format!("eden-{}", self.session_id),
            |path| {
                format!(
                    "{}:{}",
                    if self.read_only { "reading" } else { "history" },
                    path.display()
                )
            },
        )
    }
}
/// Read-only capability alone never grants ownership of a process.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum Cleanup {
    Detach,
    OwnedReader,
}
/// Existing owners and newly restored writers have different process lifetimes.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum OpenOutcome {
    Attached,
    Created,
    Restored,
    ReadOnly,
}
/// Faulty directories cannot discard valid histories from other directories.
#[derive(Clone, Debug, Serialize)]
#[allow(missing_docs)]
pub struct Directory {
    pub entries: Vec<SavedSession>,
    pub diagnostics: Vec<String>,
}
pub(crate) fn fault(code: &str, message: impl Into<String>) -> Fault {
    Fault::new(code, "session-lifecycle", message)
}
fn canonical_locator(path: &Path) -> std::io::Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            Ok(
                std::fs::canonicalize(parent)?.join(path.file_name().ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "history has no filename")
                })?),
            )
        }
        Err(error) => Err(error),
    }
}
/// Hosts and all frontend consumers share this explicit registry location.
pub fn state_dir() -> PathBuf {
    let path = std::env::var_os("EDEN_TUI_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let root = std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os(if cfg!(windows) {
                        "LOCALAPPDATA"
                    } else {
                        "HOME"
                    })
                    .map(|p| PathBuf::from(p).join(".local/state"))
                })
                .unwrap_or_else(std::env::temp_dir);
            root.join("eden/tui")
        });
    std::env::current_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(path)
}
/// An instance removes only its own registration, including during a concurrent restart.
pub struct Registration(PathBuf);
impl Registration {
    /// Publish only after the writer and endpoint have been acquired.
    pub fn new(session: &Session, endpoint: &Path) -> Result<Option<Self>, Fault> {
        let Some(history) = session.history_destination() else {
            return Ok(None);
        };
        let directory = state_dir().join("live");
        std::fs::create_dir_all(&directory).map_err(|e| fault("RegistryFailure", e.to_string()))?;
        let instance = endpoint.file_stem().unwrap_or_default().to_string_lossy();
        let path = directory.join(format!("{}-{instance}.json", session.id()));
        let value = json!({
            "history":
                canonical_locator(history).map_err(|e| fault("RegistryFailure", e.to_string()))?,
            "endpoint": std::fs::canonicalize(endpoint)
                .map_err(|e| fault("RegistryFailure", e.to_string()))?,
            "session_id": session.id(),
        });
        std::fs::write(
            &path,
            serde_json::to_vec(&value).map_err(|e| fault("RegistryFailure", e.to_string()))?,
        )
        .map_err(|e| fault("RegistryFailure", e.to_string()))?;
        Ok(Some(Self(path)))
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
/// A detached child reports a typed startup failure to its creating process.
pub fn report_start_failure(error: &(dyn std::error::Error + 'static)) {
    if let Some(path) = std::env::var_os("EDEN_HOST_STARTUP_RECEIPT") {
        let fault = error
            .downcast_ref::<Fault>()
            .cloned()
            .unwrap_or_else(|| fault("StartFailed", error.to_string()));
        if let Ok(bytes) = serde_json::to_vec(&fault) {
            let _ = std::fs::write(path, bytes);
        }
    }
}
// A cancelled startup still kills its unpublished child; published hosts are disarmed.
struct Startup {
    child: Option<tokio::process::Child>,
    endpoint: PathBuf,
    cleanup: std::sync::Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}
impl Drop for Startup {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let endpoint = self.endpoint.clone();
            let task = tokio::spawn(async move {
                let _ = child.wait().await;
                let _ = tokio::fs::remove_file(endpoint).await;
            });
            self.cleanup
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(task);
        }
    }
}
impl Lifecycle {
    /// Keep launch configuration serializable while cleanup ownership remains local to its consumer.
    pub fn new(
        executable: PathBuf,
        arguments: Vec<String>,
        cwd: PathBuf,
        state_dir: PathBuf,
        legacy_directories: Vec<PathBuf>,
    ) -> Self {
        Self {
            executable,
            arguments,
            cwd,
            state_dir,
            legacy_directories,
            startup_cleanup: Default::default(),
        }
    }
    /// Frontend shutdown calls this after cancelling requests, before reporting cleanup complete.
    pub async fn finish_startup_cleanup(&self) {
        loop {
            let tasks = std::mem::take(
                &mut *self
                    .startup_cleanup
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
            );
            if tasks.is_empty() {
                return;
            }
            for task in tasks {
                let _ = task.await;
            }
        }
    }
    /// Validate endpoint metadata and the responding host before acquiring an attachment.
    pub async fn attach(&self, endpoint: &Path, history: Option<PathBuf>) -> Result<Opened, Fault> {
        let endpoint_path = std::fs::canonicalize(endpoint)
            .map_err(|e| fault("OwnerUnavailable", e.to_string()))?;
        let endpoint = endpoint_path.as_path();
        let metadata: Value = serde_json::from_slice(
            &tokio::fs::read(endpoint)
                .await
                .map_err(|e| fault("OwnerUnavailable", e.to_string()))?,
        )
        .map_err(|e| fault("IdentityMismatch", e.to_string()))?;
        let id = metadata["session_id"]
            .as_u64()
            .ok_or_else(|| fault("IdentityMismatch", "endpoint has no Session identity"))?;
        let instance = metadata["instance"]
            .as_str()
            .filter(|value| !value.is_empty())
            .or_else(|| metadata["address"].as_str())
            .ok_or_else(|| fault("IdentityMismatch", "endpoint has no host instance"))?;
        let client = eden_tui_client::HostClient::for_instance(endpoint, id, instance)
            .map_err(|e| fault("IdentityMismatch", e.message))?;
        let state = client
            .request("GET", "/snapshot", None)
            .await
            .map_err(|e| fault("OwnerUnavailable", e.message))?;
        if state["state"]["session_id"].as_u64() != Some(id)
            || state["presentation"]["session_id"].as_u64().or_else(|| {
                state["presentation"]["session_id"]
                    .as_str()
                    .and_then(|id| id.parse().ok())
            }) != Some(id)
        {
            return Err(fault(
                "IdentityMismatch",
                "endpoint and responding host identities differ",
            ));
        }
        if state["state"]["closed"] == true && state["state"]["read_only"] != true {
            return Err(fault("OwnerUnavailable", "host has stopped"));
        }
        if state["host_instance"]
            .as_str()
            .is_some_and(|actual| actual != instance)
        {
            return Err(fault(
                "IdentityMismatch",
                "responding host instance differs from endpoint metadata",
            ));
        }
        let actual_history = state["history_destination"]
            .as_str()
            .or_else(|| state["history_path"].as_str())
            .map(PathBuf::from);
        if let (Some(expected), Some(actual)) = (&history, &actual_history)
            && std::fs::canonicalize(expected).ok() != std::fs::canonicalize(actual).ok()
        {
            return Err(fault(
                "IdentityMismatch",
                "responding host owns another history locator",
            ));
        }
        let history = history
            .or(actual_history)
            .map(|path| std::fs::canonicalize(&path).unwrap_or(path));
        Ok(Opened {
            endpoint: endpoint.into(),
            history,
            session_id: id,
            instance: instance.into(),
            outcome: OpenOutcome::Attached,
            cleanup: Cleanup::Detach,
            read_only: state["state"]["read_only"] == true,
            draft: state["state"]["draft"] == true,
            diagnostic: None,
        })
    }
    /// An invalid identity cannot silently become a new writer; stale endpoints may be ignored.
    pub async fn owner(&self, history: &Path) -> Result<Option<Opened>, Fault> {
        let history = std::fs::canonicalize(history)
            .map_err(|e| fault("HistoryUnavailable", e.to_string()))?;
        let expected = Session::describe_saved_session(history.clone())
            .await?
            .session_id;
        let entries = match std::fs::read_dir(self.state_dir.join("live")) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(fault("RegistryFailure", e.to_string())),
        };
        for entry in entries.flatten() {
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            if value["history"].as_str() != history.to_str() {
                continue;
            }
            let Some(endpoint) = value["endpoint"].as_str() else {
                continue;
            };
            match self
                .attach(Path::new(endpoint), Some(history.clone()))
                .await
            {
                Ok(opened) => {
                    if value["session_id"].as_u64() != Some(opened.session_id)
                        || expected != Some(opened.session_id)
                    {
                        return Err(fault(
                            "IdentityMismatch",
                            "registry and host identities differ",
                        ));
                    }
                    return Ok(Some(opened));
                }
                Err(error)
                    if matches!(error.code.as_str(), "IdentityMismatch" | "SessionMismatch") =>
                {
                    return Err(error);
                }
                Err(_) => continue,
            }
        }
        Ok(None)
    }
    /// Recovery targets only the selected history and never guesses a recent Session.
    pub async fn open(&self, history: &Path, reading: bool) -> Result<Opened, Fault> {
        let history = std::fs::canonicalize(history)
            .map_err(|e| fault("HistoryUnavailable", e.to_string()))?;
        if !reading {
            if let Some(owner) = self.owner(&history).await? {
                return Ok(owner);
            }
            self.writer_available(&history).await?;
        }
        match self.spawn(Some(history.clone()), reading, false).await {
            Ok(opened) => Ok(opened),
            Err(error) if !reading && error.code == "BindingIncompatible" => {
                let mut opened = self.spawn(Some(history), true, false).await?;
                opened.diagnostic = Some(error.message);
                Ok(opened)
            }
            Err(error) => Err(error),
        }
    }
    /// New tasks reserve a draft. Work admission publishes their history before execution.
    pub async fn create(&self, persist: bool) -> Result<Opened, Fault> {
        self.spawn(None, false, persist).await
    }
    async fn writer_available(&self, history: &Path) -> Result<(), Fault> {
        let path = history.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut name = path.as_os_str().to_owned();
            name.push(".lock");
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(name)
                .map_err(|e| fault("WriterConflict", e.to_string()))?;
            lock.try_lock().map_err(|e| {
                fault(
                    "WriterConflict",
                    format!("selected history still has a writer: {e}"),
                )
            })
        })
        .await
        .map_err(|e| fault("WriterConflict", e.to_string()))?
    }
    async fn spawn(
        &self,
        history: Option<PathBuf>,
        reading: bool,
        persist: bool,
    ) -> Result<Opened, Fault> {
        let directory = self.state_dir.join("hosts");
        tokio::fs::create_dir_all(&directory)
            .await
            .map_err(|e| fault("StartFailed", e.to_string()))?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| fault("StartFailed", e.to_string()))?
            .as_nanos();
        let endpoint = directory.join(format!("{stamp}-{}.json", std::process::id()));
        let log_path = endpoint.with_extension("log");
        let log =
            std::fs::File::create(&log_path).map_err(|e| fault("StartFailed", e.to_string()))?;
        let mut command = tokio::process::Command::new(&self.executable);
        let mut args = self.arguments.iter();
        while let Some(argument) = args.next() {
            if argument == "--no-session" {
                continue;
            }
            if history.is_some() && matches!(argument.as_str(), "--cwd" | "--model" | "--thinking")
            {
                args.next();
                continue;
            }
            if history.is_some()
                && matches!(argument.as_str(), "--trust-project" | "--no-trust-project")
            {
                continue;
            }
            command.arg(argument);
        }
        if history.is_none() {
            command.arg("--cwd").arg(&self.cwd);
        }
        command.env("EDEN_TUI_STATE_DIR", &self.state_dir);
        let selected = if persist && history.is_none() {
            let directory = self.cwd.join(".eden/sessions");
            tokio::fs::create_dir_all(&directory)
                .await
                .map_err(|e| fault("StartFailed", e.to_string()))?;
            Some(directory.join(format!("{stamp}-{}.jsonl", std::process::id())))
        } else {
            history.clone()
        };
        if reading {
            command.arg("read").arg(
                history
                    .as_ref()
                    .ok_or_else(|| fault("HistoryUnavailable", "reading requires a history"))?,
            );
        } else if let Some(path) = &selected {
            if history.is_none() {
                command.arg("--draft-session");
            }
            command
                .arg("--session")
                .arg(path)
                .arg(if history.is_some() {
                    "resume-live"
                } else {
                    "live"
                });
        } else {
            command.arg("--no-session").arg("live");
        }
        command.env(
            "EDEN_HOST_STARTUP_RECEIPT",
            endpoint.with_extension("startup.json"),
        );
        command
            .arg("--endpoint")
            .arg(&endpoint)
            .stdin(Stdio::null())
            .stdout(
                log.try_clone()
                    .map_err(|e| fault("StartFailed", e.to_string()))?,
            )
            .stderr(log);
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(0x0800_0200);
        let mut startup = Startup {
            child: Some(
                command
                    .spawn()
                    .map_err(|e| fault("StartFailed", e.to_string()))?,
            ),
            endpoint: endpoint.clone(),
            cleanup: self.startup_cleanup.clone(),
        };
        let child = startup
            .child
            .as_mut()
            .ok_or_else(|| fault("StartFailed", "missing startup child"))?;
        let ready = tokio::time::timeout(Duration::from_secs(30), async {
            let mut tick = tokio::time::interval(Duration::from_millis(25));
            loop {
                if let Some(status) = child
                    .try_wait()
                    .map_err(|e| fault("StartFailed", e.to_string()))?
                {
                    let reported = tokio::fs::read(endpoint.with_extension("startup.json"))
                        .await
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<Fault>(&bytes).ok());
                    return Err(reported.unwrap_or_else(|| {
                        fault(
                            "StartFailed",
                            format!("host exited {status}; see {}", log_path.display()),
                        )
                    }));
                }
                if endpoint.exists() {
                    return self.attach(&endpoint, selected.clone()).await;
                }
                tick.tick().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            Err(fault(
                "StartFailed",
                format!("host startup deadline exceeded; see {}", log_path.display()),
            ))
        });
        match ready {
            Ok(mut opened) => {
                if let Some(mut child) = startup.child.take() {
                    tokio::spawn(async move {
                        let _ = child.wait().await;
                    });
                }
                opened.outcome = if reading {
                    OpenOutcome::ReadOnly
                } else if history.is_some() {
                    OpenOutcome::Restored
                } else {
                    OpenOutcome::Created
                };
                opened.cleanup = if reading {
                    Cleanup::OwnedReader
                } else {
                    Cleanup::Detach
                };
                Ok(opened)
            }
            Err(error) => {
                // Retain ownership across the wait so cancellation still queues the reaper.
                if let Some(child) = startup.child.as_mut() {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                }
                startup.child.take();
                let _ = tokio::fs::remove_file(&endpoint).await;
                Err(error)
            }
        }
    }
    /// Stop the verified owner and acknowledge completion only after its writer releases.
    pub async fn stop(&self, history: &Path) -> Result<(), Fault> {
        let owner = self
            .owner(history)
            .await?
            .ok_or_else(|| fault("OwnerUnavailable", "selected history has no live owner"))?;
        self.stop_opened(&owner).await
    }
    /// Stop the instance reviewed by the caller; a concurrent replacement is a new target.
    pub async fn stop_opened(&self, owner: &Opened) -> Result<(), Fault> {
        let history = owner
            .history
            .as_ref()
            .ok_or_else(|| fault("HistoryUnavailable", "writer has no selected history"))?;
        let _ = history;
        self.close(owner).await
    }

    /// A failed consumer cleans its own unused start; a borrowed host or accepted run survives.
    pub async fn failed_consumer(&self, opened: &Opened) -> Result<(), Fault> {
        if opened.outcome == OpenOutcome::Attached {
            return Ok(());
        }
        let client = eden_tui_client::HostClient::for_instance(
            &opened.endpoint,
            opened.session_id,
            &opened.instance,
        )?;
        let state = client.verify().await?;
        if state.active_run.is_some()
            || !state.shell_runs.is_empty()
            || !state.command_runs.is_empty()
        {
            return Ok(());
        }
        if opened.cleanup == Cleanup::OwnedReader || opened.history.is_none() || state.draft {
            self.close(opened).await
        } else {
            self.stop_opened(opened).await
        }
    }
    /// Explicit reader cleanup never depends on read-only capability inferred from a snapshot.
    pub async fn close(&self, opened: &Opened) -> Result<(), Fault> {
        self.close_reviewed(opened, None).await
    }
    pub(crate) async fn release_draft(&self, opened: &Opened) -> Result<(), Fault> {
        let client = eden_tui_client::HostClient::for_instance(
            &opened.endpoint,
            opened.session_id,
            &opened.instance,
        )?;
        let reply = client
            .request("POST", "/discard-draft", Some(&json!({})))
            .await?;
        if reply["closing"] == true {
            self.wait_closed(opened).await?;
        }
        Ok(())
    }
    pub(crate) async fn close_reviewed(
        &self,
        opened: &Opened,
        expected: Option<eden_agent::StopExpectation>,
    ) -> Result<(), Fault> {
        if !opened.endpoint.exists() {
            return Ok(());
        }
        let client = eden_tui_client::HostClient::for_instance(
            &opened.endpoint,
            opened.session_id,
            &opened.instance,
        )?;
        client
            .request(
                "POST",
                "/shutdown",
                Some(&if let Some(expected) = expected {
                    json!({ "expected": expected })
                } else {
                    json!({})
                }),
            )
            .await?;
        self.wait_closed(opened).await
    }
    async fn wait_closed(&self, opened: &Opened) -> Result<(), Fault> {
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            loop {
                let writer_released = if let Some(history) = &opened.history {
                    opened.read_only || self.writer_available(history).await.is_ok()
                } else {
                    true
                };
                if !opened.endpoint.exists() && writer_released {
                    break;
                }
                tick.tick().await;
            }
        })
        .await
        .map_err(|_| {
            fault(
                "StopPending",
                "host cleanup has not retired its endpoint and writer",
            )
        })
    }
    pub(crate) fn catalog_directories(
        &self,
        cwd: &Path,
        explicit: Option<&Path>,
        saved: &[PathBuf],
    ) -> Vec<PathBuf> {
        let mut directories = if let Some(directory) = explicit {
            vec![cwd.join(directory)]
        } else {
            let mut directories = vec![cwd.join(".eden/sessions")];
            directories.extend(self.legacy_directories.clone());
            directories.extend(saved.iter().filter_map(|p| p.parent().map(Path::to_owned)));
            directories
        };
        directories.sort();
        directories.dedup();
        directories
    }
    /// Discovery reads metadata without an attached Session or loading any plugins.
    pub async fn discover(
        &self,
        cwd: &Path,
        explicit: Option<&Path>,
        saved: &[PathBuf],
    ) -> Directory {
        self.discover_progress(cwd, explicit, saved, |_| {}).await
    }
    /// Emit usable metadata before slower entries finish; callers keep the same canonical policy.
    pub async fn discover_progress(
        &self,
        cwd: &Path,
        explicit: Option<&Path>,
        saved: &[PathBuf],
        mut progress: impl FnMut(&Directory),
    ) -> Directory {
        let project = cwd.join(".eden/sessions");
        let directories = self.catalog_directories(cwd, explicit, saved);
        let mut result = Directory {
            entries: vec![],
            diagnostics: vec![],
        };
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.into());
        let cwd = canonical(cwd);
        for directory in directories {
            let mut scan = Session::scan_saved_sessions(directory.clone());
            while let Some(entry) = scan.next().await {
                match entry {
                    Ok(mut entry) => {
                        if explicit.is_none()
                            && entry
                                .cwd
                                .as_ref()
                                .is_some_and(|p| canonical(Path::new(p)) != cwd)
                        {
                            continue;
                        }
                        if explicit.is_none() && entry.cwd.is_none() && directory != project {
                            continue;
                        }
                        entry.path = canonical(&entry.path);
                        if !result.entries.iter().any(|r| r.path == entry.path) {
                            result.entries.push(entry);
                            result.entries.sort_by(|a, b| {
                                b.activity
                                    .or(b.modified)
                                    .cmp(&a.activity.or(a.modified))
                                    .then_with(|| b.modified_ns.cmp(&a.modified_ns))
                                    .then_with(|| a.path.cmp(&b.path))
                            });
                            progress(&result);
                        }
                    }
                    Err(error) => result
                        .diagnostics
                        .push(format!("{}: {error}", directory.display())),
                }
            }
        }
        result.entries.sort_by(|a, b| {
            b.activity
                .or(b.modified)
                .cmp(&a.activity.or(a.modified))
                .then_with(|| b.modified_ns.cmp(&a.modified_ns))
                .then_with(|| a.path.cmp(&b.path))
        });
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    fn fixture(name: &str) -> Lifecycle {
        let directory =
            std::env::temp_dir().join(format!("eden-lifecycle-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        Lifecycle {
            executable: directory.join("missing-executable"),
            arguments: vec![],
            cwd: directory.clone(),
            state_dir: directory.join("state"),
            legacy_directories: vec![],
            startup_cleanup: Default::default(),
        }
    }
    fn history(path: &Path, id: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            json!({
                "schema_version": 2,
                "transaction": [{
                    "schema_version": 2,
                    "session_id": id,
                    "sequence": 1,
                    "run_id": 0,
                    "kind": "session",
                    "payload": {
                        "cwd": path.parent().unwrap().parent().unwrap().parent().unwrap(),
                    },
                    "branch": "main",
                }],
            })
            .to_string()
                + "\n",
        )
        .unwrap();
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cancelled_start_waits_child_exit_and_writer_release() {
        use std::os::unix::fs::PermissionsExt;
        let mut service = fixture("cancelled-start");
        let executable = service.cwd.join("slow-host");
        std::fs::write(
            &executable,
            r#"#!/usr/bin/env python3
import fcntl, os, pathlib, sys, time
cwd = pathlib.Path(sys.argv[sys.argv.index('--cwd') + 1])
lock = (cwd / 'startup.lock').open('a+')
fcntl.flock(lock, fcntl.LOCK_EX)
(cwd / 'startup.pid').write_text(str(os.getpid()))
while True: time.sleep(60)
"#,
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        service.executable = executable;
        let created = service.clone();
        let task = tokio::spawn(async move { created.create(false).await });
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut tick = tokio::time::interval(Duration::from_millis(10));
            while !service.cwd.join("startup.pid").exists() {
                tick.tick().await;
            }
        })
        .await
        .unwrap();
        let pid = std::fs::read_to_string(service.cwd.join("startup.pid")).unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(service.cwd.join("startup.lock"))
            .unwrap();
        assert!(lock.try_lock().is_err());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(10), service.finish_startup_cleanup())
            .await
            .unwrap();
        assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
        lock.try_lock().unwrap();
        drop(lock);
        std::fs::remove_dir_all(service.cwd).unwrap();
    }
    #[tokio::test]
    async fn stale_registry_does_not_take_over_an_existing_writer() {
        let service = fixture("stale");
        let path = service.cwd.join(".eden/sessions/one.jsonl");
        history(&path, 7);
        let registry = service.state_dir.join("live");
        std::fs::create_dir_all(&registry).unwrap();
        std::fs::write(
            registry.join("old.json"),
            json!({
                "history": path,
                "endpoint": service.cwd.join("missing.json"),
                "session_id": 7,
            })
            .to_string(),
        )
        .unwrap();
        let lock = std::fs::File::create(path.with_extension("jsonl.lock")).unwrap();
        lock.try_lock().unwrap();
        assert!(service.owner(&path).await.unwrap().is_none());
        assert_eq!(
            service.open(&path, false).await.unwrap_err().code,
            "WriterConflict"
        );
        drop(lock);
        std::fs::remove_dir_all(service.cwd).unwrap();
    }
    #[tokio::test]
    async fn valid_host_for_another_persistent_identity_is_rejected() {
        let service = fixture("identity");
        let path = service.cwd.join(".eden/sessions/one.jsonl");
        history(&path, 7);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = service.cwd.join("endpoint.json");
        std::fs::write(
            &endpoint,
            json!({
                "address": listener.local_addr().unwrap().to_string(),
                "token": "fixture",
                "session_id": 9,
            })
            .to_string(),
        )
        .unwrap();
        let registry = service.state_dir.join("live");
        std::fs::create_dir_all(&registry).unwrap();
        std::fs::write(
            registry.join("bad.json"),
            json!({ "history": path, "endpoint": endpoint, "session_id": 9 }).to_string(),
        )
        .unwrap();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            let body = json!({
                "ok": true,
                "result": {
                    "state": { "session_id": 9, "closed": false },
                    "presentation": { "session_id": 9 },
                },
            })
            .to_string();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        assert_eq!(
            service.owner(&path).await.unwrap_err().code,
            "IdentityMismatch"
        );
        peer.await.unwrap();
        std::fs::remove_dir_all(service.cwd).unwrap();
    }
    #[tokio::test]
    async fn directory_faults_keep_sorted_usable_rows_and_bad_files_visible() {
        let mut service = fixture("directory");
        let directory = service.cwd.join(".eden/sessions");
        let old = directory.join("z-old.jsonl");
        history(&old, 7);
        let new = directory.join("a-new.jsonl");
        history(&new, 8);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&new)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(2))
            .unwrap();
        std::fs::write(directory.join("broken.jsonl"), "{broken").unwrap();
        let invalid = service.cwd.join("not-a-directory");
        std::fs::write(&invalid, "fixture").unwrap();
        service.legacy_directories.push(invalid);
        let mut received = vec![];
        let result = service
            .discover_progress(&service.cwd, None, &[], |partial| {
                received.push(partial.entries.len())
            })
            .await;
        assert_eq!(received, [1, 2, 3]);
        assert_eq!(result.entries.len(), 3);
        assert_eq!(
            result
                .entries
                .iter()
                .filter_map(|e| e.session_id)
                .collect::<Vec<_>>(),
            [8, 7]
        );
        assert_eq!(result.diagnostics.len(), 1);
        assert!(result.entries.iter().any(|e| e.diagnostic.is_some()));
        std::fs::remove_dir_all(service.cwd).unwrap();
    }
}
