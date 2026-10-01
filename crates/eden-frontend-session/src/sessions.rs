//! The Grok picker selects explicit Eden histories; the host opens or attaches them.
use crate::{Adapter, fault, projection::text};
use eden_protocol::Fault;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;

pub(crate) struct Server {
    root: Mutex<Arc<Adapter>>,
    lifecycle: eden_session_lifecycle::Lifecycle,
    output: tokio::sync::mpsc::UnboundedSender<Value>,
    copies: Mutex<BTreeMap<String, eden_agent::CopyPlan>>,
    pub(crate) forms: crate::forms::Forms,
    sessions: Mutex<BTreeMap<String, Arc<Adapter>>>,
    saved: std::sync::Mutex<BTreeMap<String, String>>,
    opening: Mutex<()>,
    followers: Mutex<tokio::task::JoinSet<()>>,
}

impl Server {
    pub(crate) fn new(
        root: Arc<Adapter>,
        lifecycle: eden_session_lifecycle::Lifecycle,
        opened: eden_session_lifecycle::Opened,
    ) -> Self {
        let mut saved = BTreeMap::new();
        if let Some(path) = opened.history {
            saved.insert(root.identity.clone(), path.to_string_lossy().into_owned());
        }
        Self {
            copies: Default::default(),
            output: root.output.clone(),
            lifecycle,
            sessions: Mutex::new(BTreeMap::from([(root.identity.clone(), root.clone())])),
            root: Mutex::new(root),
            forms: Default::default(),
            saved: std::sync::Mutex::new(saved),
            opening: Mutex::new(()),
            followers: Mutex::new(tokio::task::JoinSet::new()),
        }
    }

    pub(crate) async fn close_reader(
        &self,
        opened: &eden_session_lifecycle::Opened,
    ) -> Result<(), Fault> {
        self.lifecycle.close(opened).await
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

    pub(crate) async fn finish_startup_cleanup(&self) {
        self.lifecycle.finish_startup_cleanup().await;
    }
    pub(crate) async fn stop_followers(&self) {
        self.followers.lock().await.shutdown().await;
    }

    pub(crate) async fn finish_load(&self, params: &Value, success: bool) {
        let root = self.root().await;
        let id = params["sessionId"].as_str().unwrap_or(&root.identity);
        let selected = self.sessions.lock().await.get(id).cloned();
        let Some(selected) = selected else {
            return;
        };
        selected
            .load_pending
            .store(false, std::sync::atomic::Ordering::Release);
        selected.loaded.notify_one();
        let history = selected
            .opened
            .as_ref()
            .and_then(|opened| opened.history.as_ref());
        let retired = {
            let mut sessions = self.sessions.lock().await;
            let keys = sessions
                .iter()
                .filter(|(key, adapter)| {
                    if Arc::ptr_eq(adapter, &root) {
                        return false;
                    }
                    if !success {
                        return key.as_str() == id;
                    }
                    key.as_str() != id
                        && adapter.lease.load(std::sync::atomic::Ordering::Acquire) != 0
                        && history.is_some()
                        && adapter
                            .opened
                            .as_ref()
                            .and_then(|opened| opened.history.as_ref())
                            == history
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| sessions.remove(&key))
                .collect::<Vec<_>>()
        };
        for adapter in retired {
            adapter
                .stopped
                .store(true, std::sync::atomic::Ordering::Release);
            let lease = adapter.lease.swap(0, std::sync::atomic::Ordering::AcqRel);
            if lease != 0 {
                let _ = adapter.client.detach(lease).await;
            }
            if let Some(opened) = &adapter.opened
                && opened.cleanup == eden_session_lifecycle::Cleanup::OwnedReader
                && self.lifecycle.close(opened).await.is_err()
            {
                // Keep cleanup ownership even when the retired reader cannot be reached yet.
                self.sessions
                    .lock()
                    .await
                    .insert(adapter.identity.clone(), adapter);
            }
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
                && (method != "session/load" || adapter.client.verify().await.is_ok())
            {
                return Ok(adapter);
            }
            if method.is_empty() {
                return Ok(adapter);
            }
        }
        if method != "session/load" {
            return Err(fault("Unknown Eden session"));
        }
        let base = id.rsplit_once("::view-").map_or(id, |(base, _)| base);
        let cached = self.sessions.lock().await.get(base).cloned();
        if let Some(adapter) = cached
            && adapter.client.verify().await.is_ok()
        {
            let mut view = Adapter::new(
                adapter.endpoint.clone(),
                self.output.clone(),
                Some(id.into()),
            )
            .await?;
            view.opened = adapter.opened.clone().map(|mut opened| {
                opened.cleanup = eden_session_lifecycle::Cleanup::Detach;
                opened
            });
            let view = Arc::new(view);
            self.sessions.lock().await.insert(id.into(), view.clone());
            return Ok(view);
        }
        let path = self
            .saved
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(base)
            .cloned()
            .ok_or_else(|| fault("Choose a history from the Eden session picker first"))?;
        self.open_adapter(&path, id, base.starts_with("reading:"))
            .await
    }

    pub(crate) async fn open_history(&self, path: &str, reading: bool) -> Result<String, Fault> {
        let info = eden_agent::Session::describe_saved_session(PathBuf::from(path)).await?;
        let id = if reading {
            format!("reading:{}", info.path.display())
        } else {
            if info.session_id.is_none() {
                return Err(fault("History has no valid Session identity"));
            }
            format!(
                "history:{}",
                std::fs::canonicalize(&info.path)
                    .map_err(|e| fault(e.to_string()))?
                    .display()
            )
        };
        self.saved
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id.clone(), path.into());
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
        if let Some(adapter) = self.sessions.lock().await.get(id).cloned()
            && !adapter.stopped.load(std::sync::atomic::Ordering::Acquire)
            && adapter.client.verify().await.is_ok()
        {
            return Ok(adapter);
        }
        crate::trace(json!({
            "direction": "phase",
            "phase": "connection",
            "edge": "start",
            "session": id,
        }));
        let opened = self
            .lifecycle
            .open(std::path::Path::new(path), reading)
            .await?;
        crate::trace(json!({
            "direction": "phase",
            "phase": "connection",
            "edge": "end",
            "session": id,
        }));
        let mut adapter = match Adapter::new(
            opened.endpoint.clone(),
            self.output.clone(),
            Some(id.into()),
        )
        .await
        {
            Ok(adapter) => adapter,
            Err(error) => {
                self.lifecycle.failed_consumer(&opened).await?;
                return Err(error);
            }
        };
        adapter.reading_diagnostic = opened.diagnostic.clone();
        adapter.opened = Some(opened);
        let adapter = Arc::new(adapter);
        if let Some(old) = self
            .sessions
            .lock()
            .await
            .insert(id.into(), adapter.clone())
        {
            old.stopped
                .store(true, std::sync::atomic::Ordering::Release);
            let lease = old.lease.load(std::sync::atomic::Ordering::Acquire);
            if lease != 0 {
                let _ = old.client.detach(lease).await;
            }
            if let Some(opened) = &old.opened
                && opened.cleanup == eden_session_lifecycle::Cleanup::OwnedReader
            {
                self.lifecycle.close(opened).await?;
            }
        }
        Ok(adapter)
    }
    pub(crate) async fn owner(&self, path: &str) -> Result<eden_session_lifecycle::Opened, Fault> {
        self.lifecycle
            .owner(std::path::Path::new(path))
            .await?
            .ok_or_else(|| fault("This history has no live owner"))
    }
    pub(crate) async fn stop_history(
        &self,
        owner: &eden_session_lifecycle::Opened,
    ) -> Result<(), Fault> {
        let _opening = self.opening.lock().await;
        self.lifecycle.stop_opened(owner).await?;
        for adapter in self.sessions.lock().await.values() {
            if adapter.endpoint == owner.endpoint {
                adapter
                    .stopped
                    .store(true, std::sync::atomic::Ordering::Release);
            }
        }
        Ok(())
    }
    pub(crate) async fn create(&self, owner: &Adapter) -> Result<String, Fault> {
        let mut lifecycle = self.lifecycle.clone();
        let cwd = PathBuf::from(&owner.view.lock().await.snapshot.state.cwd);
        if cwd != lifecycle.cwd {
            lifecycle.arguments.retain(|argument| {
                !matches!(argument.as_str(), "--trust-project" | "--no-trust-project")
            });
        }
        lifecycle.cwd = cwd;
        let opened = lifecycle.create(true).await?;
        let mut adapter = match Adapter::new(
            opened.endpoint.clone(),
            self.output.clone(),
            Some(opened.view_key()),
        )
        .await
        {
            Ok(adapter) => adapter,
            Err(error) => {
                self.lifecycle.failed_consumer(&opened).await?;
                return Err(error);
            }
        };
        adapter.opened = Some(opened.clone());
        let adapter = Arc::new(adapter);
        let identity = adapter.identity.clone();
        if let Some(path) = opened.history {
            self.saved
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(identity.clone(), path.to_string_lossy().into_owned());
        }
        self.sessions.lock().await.insert(identity.clone(), adapter);
        Ok(identity)
    }
    pub(crate) async fn info(&self, path: &str) -> Result<Value, Fault> {
        let mut info =
            serde_json::to_value(eden_agent::Session::describe_saved_session(path.into()).await?)
                .map_err(|e| fault(e.to_string()))?;
        let owner = self.lifecycle.owner(std::path::Path::new(path)).await?;
        info["owner"] = json!(owner.as_ref().map(|opened| &opened.endpoint));
        info["status"] = json!(if owner.is_some() {
            "live writer"
        } else {
            "stopped; binding checked on open"
        });
        Ok(info)
    }
    pub(crate) async fn manage(&self, route: &str, body: Value) -> Result<Value, Fault> {
        let path = |key: &str| {
            body[key]
                .as_str()
                .map(PathBuf::from)
                .ok_or_else(|| fault(format!("Missing {key}")))
        };
        match route {
            "/manage/rename" => {
                let adapter = self
                    .selected(
                        path("path")?
                            .to_str()
                            .ok_or_else(|| fault("Invalid history path"))?,
                    )
                    .await?;
                let reply = adapter.post("/manage/metadata", body).await?;
                adapter
                    .complete(
                        reply["run_id"]
                            .as_u64()
                            .ok_or_else(|| fault("Missing metadata operation"))?,
                    )
                    .await
            }
            "/manage/tree" => Ok(json!(
                eden_agent::Session::saved_session_tree(path("path")?).await?
            )),
            "/manage/delete" => {
                if body["confirmed"] != true {
                    return Err(fault("Confirm the exact selected history"));
                }
                eden_agent::Session::delete_saved_session(
                    path("path")?,
                    body["expected_session"]
                        .as_u64()
                        .ok_or_else(|| fault("Missing selected identity"))?,
                )
                .await?;
                Ok(json!({ "deleted": body["path"] }))
            }
            "/manage/copy/preview" => {
                let composition = self
                    .lifecycle
                    .arguments
                    .windows(2)
                    .find(|args| args[0] == "--composition")
                    .map(|args| PathBuf::from(&args[1]))
                    .unwrap_or_else(|| {
                        self.lifecycle
                            .executable
                            .parent()
                            .and_then(std::path::Path::parent)
                            .unwrap_or(std::path::Path::new("."))
                            .join("composition.json")
                    });
                let plan = eden_agent::Session::plan_copy(
                    composition,
                    eden_agent::CopyOptions {
                        source: path("source")?,
                        destination: path("destination")?,
                        kind: serde_json::from_value(body["kind"].clone())
                            .map_err(|e| fault(e.to_string()))?,
                        target: body["target"].as_u64(),
                        cwd: body["cwd"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .map(PathBuf::from),
                        public_only: false,
                    },
                )
                .await?;
                let id = body["request_id"]
                    .as_str()
                    .ok_or_else(|| fault("Missing preview identity"))?
                    .to_owned();
                let reply = json!({ "preview_id": id, "plan": plan });
                let mut copies = self.copies.lock().await;
                if copies.len() >= 8 {
                    return Err(fault("Discard an unused copy preview"));
                }
                copies.insert(id, plan);
                Ok(reply)
            }
            "/manage/copy/discard" => {
                self.copies.lock().await.remove(text(&body["preview_id"]));
                Ok(json!({ "discarded": true }))
            }
            "/manage/copy/apply" => {
                if body["confirmed"] != true {
                    return Err(fault("Confirm the reviewed copy plan"));
                }
                let plan = self
                    .copies
                    .lock()
                    .await
                    .remove(text(&body["preview_id"]))
                    .ok_or_else(|| fault("Preview expired; prepare the copy again"))?;
                Ok(json!({ "created": eden_agent::Session::apply_copy(plan).await? }))
            }
            _ => Err(fault("Unknown independent history action")),
        }
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

    fn directory_rows(
        directory: &eden_session_lifecycle::Directory,
        params: &Value,
        current_history: Option<&std::path::Path>,
    ) -> Vec<Value> {
        let query = text(&params["query"]).to_lowercase();
        let mut rows = vec![];
        for entry in &directory.entries {
            let path = entry.path.to_string_lossy().into_owned();
            let id = format!("history:{path}");
            let current = current_history == Some(entry.path.as_path());
            let content = if entry.diagnostic.is_some() {
                "History needs inspection"
            } else if !entry.has_content {
                if current {
                    "Current empty session"
                } else {
                    "Empty session"
                }
            } else if current {
                "Current session"
            } else {
                "Saved session"
            };
            let title = if entry.has_name {
                &entry.name
            } else {
                entry.summary.as_deref().unwrap_or(&entry.name)
            };
            let summary = format!(
                "{content} · {title} · {}",
                entry.path.file_stem().unwrap_or_default().to_string_lossy()
            );
            if !query.is_empty() && !format!("{summary} {path}").to_lowercase().contains(&query) {
                continue;
            }
            rows.push(json!({
                "sessionId": id,
                "summary": summary,
                "cwd": entry.cwd,
                "source": "local",
                "updatedAtUnix": entry.modified,
                "numMessages": entry.messages,
                "_meta": {
                    "edenDiagnostic": entry.diagnostic,
                    "edenTags": entry.tags,
                    "edenSessionId": entry.session_id,
                    "edenPath": path,
                    "edenHasContent": entry.has_content,
                    "edenCurrent": current,
                },
            }));
        }
        rows
    }
    pub(crate) async fn list(&self, params: &Value) -> Result<Value, Fault> {
        let root = self.root().await;
        let owner = if let Some(id) = params["sessionId"]
            .as_str()
            .or_else(|| params["filter_session_id"].as_str())
        {
            self.sessions.lock().await.get(id).cloned().unwrap_or(root)
        } else {
            root
        };
        let view = owner.view.lock().await;
        let cwd = if view.snapshot.state.cwd.is_empty() {
            self.lifecycle.cwd.clone()
        } else {
            PathBuf::from(&view.snapshot.state.cwd)
        };
        drop(view);
        let saved = self
            .saved
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        let explicit = params["directory"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(std::path::Path::new);
        let current_history = owner
            .opened
            .as_ref()
            .and_then(|opened| opened.history.as_deref());
        crate::trace(json!({
            "direction": "phase",
            "phase": "directory",
            "edge": "start",
            "cwd": cwd,
            "picker": params["_meta"]["edenPicker"],
        }));
        let mut emitted = 0;
        let directory = self
            .lifecycle
            .discover_progress(&cwd, explicit, &saved, |partial| {
                let rows = Self::directory_rows(partial, params, current_history);
                for row in &rows {
                    self.saved
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .insert(
                            text(&row["sessionId"]).into(),
                            text(&row["_meta"]["edenPath"]).into(),
                        );
                }
                if (emitted == 0 || partial.entries.len() % 16 == 0)
                    && !params["_meta"]["edenPicker"].is_null()
                {
                    let _ = self.output.send(json!({
                        "jsonrpc": "2.0",
                        "method": "_eden/session/list_progress",
                        "params": {
                            "picker": params["_meta"]["edenPicker"],
                            "sessions": rows,
                            "_meta": { "edenDiagnostics": partial.diagnostics },
                        },
                    }));
                    emitted += 1;
                }
            })
            .await;
        crate::trace(json!({
            "direction": "phase",
            "phase": "directory",
            "edge": "end",
            "picker": params["_meta"]["edenPicker"],
            "entries": directory.entries.len(),
            "progress_batches": emitted,
        }));
        let rows = Self::directory_rows(&directory, params, current_history);
        for row in &rows {
            self.saved
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(
                    text(&row["sessionId"]).into(),
                    text(&row["_meta"]["edenPath"]).into(),
                );
        }
        Ok(json!({
            "sessions": rows,
            "partial": !directory.diagnostics.is_empty(),
            "_meta": { "edenDiagnostics": directory.diagnostics },
        }))
    }
}
