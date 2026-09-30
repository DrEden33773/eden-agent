//! The Grok picker selects explicit Eden histories; the host opens or attaches them.
use crate::{Adapter, fault, projection::text};
use eden_protocol::Fault;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;

pub(crate) struct Server {
    pub(crate) root: Arc<Adapter>,
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
            root,
            forms: Default::default(),
            saved: Mutex::new(saved),
            opening: Mutex::new(()),
            followers: Mutex::new(tokio::task::JoinSet::new()),
        }
    }

    pub(crate) async fn all(&self) -> Vec<Arc<Adapter>> {
        self.sessions.lock().await.values().cloned().collect()
    }

    pub(crate) async fn follow_all(&self) {
        let adapters = self.all().await;
        let mut followers = self.followers.lock().await;
        while followers.try_join_next().is_some() {}
        for adapter in adapters {
            if adapter.lease.load(std::sync::atomic::Ordering::Relaxed) != 0
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
        let id = params["sessionId"].as_str().unwrap_or(&self.root.identity);
        if let Some(adapter) = self.sessions.lock().await.get(id) {
            adapter
                .load_pending
                .store(false, std::sync::atomic::Ordering::Release);
            adapter.loaded.notify_one();
        }
    }

    pub(crate) async fn target(&self, request: &Value) -> Result<Arc<Adapter>, Fault> {
        let method = text(&request["method"]).trim_start_matches('_');
        if method == "x.ai/session/list" {
            return Ok(self.root.clone());
        }
        let Some(id) = request["params"]["sessionId"]
            .as_str()
            .or_else(|| request["params"]["filter_session_id"].as_str())
            .or_else(|| request["params"]["session_id"].as_str())
        else {
            return Ok(self.root.clone());
        };
        if let Some(adapter) = self.sessions.lock().await.get(id).cloned() {
            return Ok(adapter);
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
        if let Some(adapter) = self.sessions.lock().await.get(id).cloned() {
            return Ok(adapter);
        }
        if self.root.lease.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            let lease = self.root.client.attach("tui").await?;
            self.root
                .lease
                .store(lease, std::sync::atomic::Ordering::Relaxed);
        }
        let opened = self
            .root
            .post(
                "/manage/open",
                json!({ "request_id": self.root.request_id(), "path": path, "read_only": reading }),
            )
            .await;
        let (opened, diagnostic) = match opened {
            Ok(value) => (value, None),
            Err(error) if !reading => {
                let opened = self
                    .root
                    .post(
                        "/manage/open",
                        json!({
                            "request_id": self.root.request_id(),
                            "path": path,
                            "read_only": true,
                        }),
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
            self.root.output.clone(),
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

    pub(crate) async fn list(&self, params: &Value) -> Result<Value, Fault> {
        let cwd = self.root.client.snapshot().await?.state.cwd;
        let mut directories = vec![PathBuf::from(&cwd).join(".eden/sessions")];
        if let Some(parent) = self.root.endpoint.parent() {
            directories.push(parent.to_path_buf());
        }
        if let Some(directory) = std::env::var_os("EDEN_GROK_SESSION_DIR") {
            directories.push(directory.into());
        }
        directories.sort();
        directories.dedup();
        let mut rows = BTreeMap::new();
        let query = text(&params["query"]).to_lowercase();
        for directory in directories {
            if !directory.is_dir() {
                continue;
            }
            let entries = self
                .root
                .post("/manage/sessions", json!({ "directory": directory }))
                .await?;
            for entry in entries.as_array().into_iter().flatten() {
                let path = text(&entry["path"]);
                if path.is_empty() {
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
                        "cwd": cwd,
                        "source": "local",
                        "updatedAtUnix": entry["modified"],
                        "numMessages": entry["records"],
                        "_meta": { "edenDiagnostic": entry["diagnostic"] },
                    }),
                );
            }
        }
        Ok(json!({ "sessions": rows.into_values().collect::<Vec<_>>(), "partial": false }))
    }
}
