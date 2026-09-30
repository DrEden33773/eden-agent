//! The Grok picker selects explicit Eden histories; the host opens or attaches them.
use crate::{Adapter, fault, projection::text};
use eden_protocol::Fault;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;

pub(crate) struct Server {
    root: Mutex<Arc<Adapter>>,
    pub(crate) forms: crate::forms::Forms,
    sessions: Mutex<BTreeMap<String, Arc<Adapter>>>,
    saved: Mutex<BTreeMap<String, String>>,
    opening: Mutex<()>,
    followers: Mutex<tokio::task::JoinSet<()>>,
}

impl Server {
    pub(crate) fn new(root: Arc<Adapter>) -> Self {
        let mut saved = BTreeMap::new();
        if let Some(path) = std::env::var_os("EDEN_GROK_INITIAL_HISTORY") {
            let path = PathBuf::from(path).to_string_lossy().into_owned();
            saved.insert(format!("history:{path}"), path);
        }
        Self {
            sessions: Mutex::new(BTreeMap::from([(root.identity.clone(), root.clone())])),
            root: Mutex::new(root),
            forms: Default::default(),
            saved: Mutex::new(saved),
            opening: Mutex::new(()),
            followers: Mutex::new(tokio::task::JoinSet::new()),
        }
    }

    pub(crate) async fn root(&self) -> Arc<Adapter> {
        self.root.lock().await.clone()
    }

    pub(crate) async fn all(&self) -> Vec<Arc<Adapter>> {
        self.sessions.lock().await.values().cloned().collect()
    }

    pub(crate) async fn follow_all(&self) {
        let adapters = self.all().await;
        let mut followers = self.followers.lock().await;
        while followers.try_join_next().is_some() {}
        for adapter in adapters {
            if !adapter.stopped.load(std::sync::atomic::Ordering::Acquire)
                && adapter.lease.load(std::sync::atomic::Ordering::Relaxed) != 0
                && !adapter
                    .following
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                followers.spawn(adapter.follow());
            }
        }
    }

    pub(crate) async fn stop_followers(&self) {
        self.followers.lock().await.shutdown().await;
    }

    pub(crate) async fn finish_load(&self, params: &Value) {
        let root = self.root().await;
        let id = params["sessionId"].as_str().unwrap_or(&root.identity);
        if let Some(adapter) = self.sessions.lock().await.get(id) {
            adapter
                .load_pending
                .store(false, std::sync::atomic::Ordering::Release);
            adapter.loaded.notify_one();
        }
    }

    pub(crate) async fn target(&self, request: &Value) -> Result<Arc<Adapter>, Fault> {
        let root = self.root().await;
        let method = text(&request["method"]).trim_start_matches('_');
        if method == "x.ai/session/list" {
            return Ok(root.clone());
        }
        let Some(id) = request["params"]["sessionId"]
            .as_str()
            .or_else(|| request["params"]["filter_session_id"].as_str())
            .or_else(|| request["params"]["session_id"].as_str())
        else {
            return Ok(root.clone());
        };
        if let Some(adapter) = self.sessions.lock().await.get(id).cloned() {
            if !adapter.stopped.load(std::sync::atomic::Ordering::Acquire)
                && adapter.endpoint.exists()
            {
                return Ok(adapter);
            }
            if method.is_empty() {
                return Ok(root);
            }
        }
        if method != "session/load" {
            return Err(fault("Unknown Eden session"));
        }
        let path = self
            .saved
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| fault("Choose a history from the Eden session picker first"))?;
        self.open_adapter(&path, id, false).await
    }

    pub(crate) async fn open_history(&self, path: &str, reading: bool) -> Result<String, Fault> {
        let id = if reading {
            format!("reading:{path}")
        } else {
            format!("history:{path}")
        };
        self.saved.lock().await.insert(id.clone(), path.into());
        self.open_adapter(path, &id, reading).await?;
        Ok(id)
    }

    async fn open_adapter(
        &self,
        path: &str,
        id: &str,
        reading: bool,
    ) -> Result<Arc<Adapter>, Fault> {
        let _opening = self.opening.lock().await;
        let root = self.root().await;
        if let Some(adapter) = self.sessions.lock().await.get(id).cloned()
            && !adapter.stopped.load(std::sync::atomic::Ordering::Acquire)
            && adapter.endpoint.exists()
        {
            return Ok(adapter);
        }
        if root.lease.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            let lease = root.client.attach("tui").await?;
            root.lease
                .store(lease, std::sync::atomic::Ordering::Relaxed);
        }
        let opened = root
            .post(
                "/manage/open",
                json!({ "request_id": root.request_id(), "path": path, "read_only": reading }),
            )
            .await;
        let (opened, diagnostic) = match opened {
            Ok(value) => (value, None),
            Err(error) if !reading => {
                let opened = root
                    .post(
                        "/manage/open",
                        json!({ "request_id": root.request_id(), "path": path, "read_only": true }),
                    )
                    .await?;
                (opened, Some(error.message))
            }
            Err(error) => return Err(error),
        };
        let endpoint = opened["endpoint"]
            .as_str()
            .ok_or_else(|| fault("Host did not return an endpoint"))?;
        let mut adapter = Adapter::new(
            PathBuf::from(endpoint),
            root.output.clone(),
            Some(id.into()),
        )
        .await?;
        adapter.owns_reader = adapter.view.lock().await.snapshot.state.read_only;
        adapter.reading_diagnostic = diagnostic;
        let adapter = Arc::new(adapter);
        self.sessions
            .lock()
            .await
            .insert(id.into(), adapter.clone());
        Ok(adapter)
    }

    pub(crate) async fn stop_history(&self, path: &str) -> Result<(), Fault> {
        let _opening = self.opening.lock().await;
        let root = self.root().await;
        let info = root.post("/manage/info", json!({ "path": path })).await?;
        let endpoint = info["owner"]
            .as_str()
            .ok_or_else(|| fault("This history has no live writer"))?;
        if *endpoint == root.endpoint {
            let opened = root
                .post(
                    "/manage/new",
                    json!({ "request_id": root.request_id(), "management_only": true }),
                )
                .await?;
            let replacement = opened["endpoint"]
                .as_str()
                .ok_or_else(|| fault("Missing management endpoint"))?;
            let mut replacement =
                Adapter::new(PathBuf::from(replacement), root.output.clone(), None).await?;
            replacement.owns_reader = true;
            replacement.lease.store(
                replacement.client.attach("tui").await?,
                std::sync::atomic::Ordering::Relaxed,
            );
            let replacement = Arc::new(replacement);
            self.sessions
                .lock()
                .await
                .insert(replacement.identity.clone(), replacement.clone());
            *self.root.lock().await = replacement;
        }
        eden_tui_client::call(
            std::path::Path::new(endpoint),
            "POST",
            "/shutdown",
            Some(&json!({ "session_id": info["session_id"] })),
        )
        .await?;
        for (id, adapter) in self.sessions.lock().await.iter() {
            if adapter.endpoint == *endpoint {
                adapter
                    .stopped
                    .store(true, std::sync::atomic::Ordering::Release);
                self.saved.lock().await.insert(id.clone(), path.into());
            }
        }
        // Shutdown acknowledgement precedes cleanup. Resume must wait for the original
        // writer lock, not assume that disappearance of an endpoint means writer exit.
        let mut lock_path = std::ffi::OsString::from(path);
        lock_path.push(".lock");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(|e| fault(e.to_string()))?;
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if lock.try_lock().is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| {
            fault("Host cleanup is still holding the writer lock; resume after cleanup completes")
        })?;
        Ok(())
    }

    pub(crate) async fn create(&self, owner: &Adapter) -> Result<String, Fault> {
        let root = self.root().await;
        let snapshot = owner.client.snapshot().await?;
        let cwd = snapshot
            .history
            .iter()
            .find(|record| record.kind == "session")
            .and_then(|record| record.payload["cwd"].as_str())
            .unwrap_or(&snapshot.state.cwd);
        let opened = root
            .post(
                "/manage/new",
                json!({ "request_id": owner.request_id(), "cwd": cwd }),
            )
            .await?;
        let endpoint = opened["endpoint"]
            .as_str()
            .ok_or_else(|| fault("Missing new Session endpoint"))?;
        let adapter =
            Arc::new(Adapter::new(PathBuf::from(endpoint), root.output.clone(), None).await?);
        let identity = adapter.identity.clone();
        self.sessions.lock().await.insert(identity.clone(), adapter);
        Ok(identity)
    }

    pub(crate) async fn selected(&self, path: &str) -> Result<Arc<Adapter>, Fault> {
        let id = self.open_history(path, false).await?;
        self.sessions
            .lock()
            .await
            .get(&id)
            .cloned()
            .ok_or_else(|| fault("Session unavailable"))
    }

    pub(crate) async fn list(&self, params: &Value) -> Result<Value, Fault> {
        let root = self.root().await;
        let id = params["sessionId"]
            .as_str()
            .or_else(|| params["filter_session_id"].as_str());
        let owner = if let Some(id) = id {
            self.sessions
                .lock()
                .await
                .get(id)
                .filter(|adapter| !adapter.stopped.load(std::sync::atomic::Ordering::Acquire))
                .cloned()
                .unwrap_or_else(|| root.clone())
        } else {
            root.clone()
        };
        let snapshot = owner.client.snapshot().await?;
        let cwd = if snapshot.state.cwd.is_empty() {
            snapshot
                .history
                .iter()
                .find(|record| record.kind == "session")
                .and_then(|record| record.payload["cwd"].as_str())
                .unwrap_or_default()
                .to_owned()
        } else {
            snapshot.state.cwd.clone()
        };
        let mut directories = vec![PathBuf::from(&cwd).join(".eden/sessions")];
        if let Some(parent) = root.endpoint.parent() {
            directories.push(parent.to_path_buf());
        }
        if let Some(directory) = std::env::var_os("EDEN_GROK_SESSION_DIR") {
            directories.push(directory.into());
        }
        if let Some(legacy) = std::env::var_os("EDEN_GROK_LEGACY_SESSIONS")
            && let Ok(children) = std::fs::read_dir(legacy)
        {
            directories.extend(
                children
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.is_dir()),
            );
        }
        let explicit_directory = params["directory"].as_str().filter(|s| !s.is_empty());
        if let Some(directory) = explicit_directory {
            directories = vec![PathBuf::from(&cwd).join(directory)];
        }
        if explicit_directory.is_none() {
            directories.extend(self.saved.lock().await.values().filter_map(|path| {
                PathBuf::from(path)
                    .parent()
                    .map(std::path::Path::to_path_buf)
            }));
        }
        directories.sort();
        directories.dedup();
        let mut rows = BTreeMap::new();
        let query = text(&params["query"]).to_lowercase();
        for directory in directories {
            if !directory.is_dir() {
                continue;
            }
            let entries = root
                .post("/manage/sessions", json!({ "directory": directory }))
                .await?;
            for entry in entries.as_array().into_iter().flatten() {
                let path = text(&entry["path"]);
                if path.is_empty() {
                    continue;
                }
                let recorded_cwd = entry["cwd"].as_str();
                if explicit_directory.is_none()
                    && recorded_cwd.is_some_and(|recorded| !same_cwd(recorded, &cwd))
                {
                    continue;
                }
                // Unknown bindings outside the project directory cannot be attributed to this project.
                if explicit_directory.is_none()
                    && recorded_cwd.is_none()
                    && directory != PathBuf::from(&cwd).join(".eden/sessions")
                    && Some(directory.as_path()) != root.endpoint.parent()
                {
                    continue;
                }
                let name = text(&entry["name"]);
                if !query.is_empty() && !format!("{name} {path}").to_lowercase().contains(&query) {
                    continue;
                }
                let id = format!("history:{path}");
                self.saved.lock().await.insert(id.clone(), path.into());
                rows.insert(
                    path.to_owned(),
                    json!({
                        "sessionId": id,
                        "summary": name,
                        "cwd": recorded_cwd,
                        "source": "local",
                        "updatedAtUnix": entry["modified"],
                        "numMessages": entry["records"],
                        "_meta": {
                            "edenDiagnostic": entry["diagnostic"],
                            "edenTags": entry["tags"],
                            "edenSessionId": entry["session_id"],
                            "edenPath": path,
                        },
                    }),
                );
            }
        }
        Ok(json!({ "sessions": rows.into_values().collect::<Vec<_>>(), "partial": false }))
    }
}

fn same_cwd(left: &str, right: &str) -> bool {
    let canonical =
        |path: &str| std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    canonical(left) == canonical(right)
}
