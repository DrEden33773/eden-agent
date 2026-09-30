//! The Grok picker selects explicit Eden histories; the host opens or attaches them.
use crate::{Adapter, fault, projection::text};
use eden_protocol::Fault;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;

pub(crate) struct Server {
    pub(crate) root: Arc<Adapter>,
    sessions: Mutex<BTreeMap<String, Arc<Adapter>>>,
    saved: Mutex<BTreeMap<String, String>>,
}

impl Server {
    pub(crate) fn new(root: Arc<Adapter>) -> Self {
        Self {
            sessions: Mutex::new(BTreeMap::from([(root.identity.clone(), root.clone())])),
            root,
            saved: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) async fn all(&self) -> Vec<Arc<Adapter>> {
        self.sessions.lock().await.values().cloned().collect()
    }

    pub(crate) async fn target(&self, request: &Value) -> Result<Arc<Adapter>, Fault> {
        let method = text(&request["method"]).trim_start_matches('_');
        if method == "x.ai/session/list" {
            return Ok(self.root.clone());
        }
        let Some(id) = request["params"]["sessionId"]
            .as_str()
            .or_else(|| request["params"]["filter_session_id"].as_str())
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
        let opened = self
            .root
            .post(
                "/manage/open",
                json!({ "request_id": self.root.request_id(), "path": path, "read_only": false }),
            )
            .await?;
        let endpoint = opened["endpoint"]
            .as_str()
            .ok_or_else(|| fault("Host did not return an endpoint"))?;
        let adapter = Arc::new(
            Adapter::new(
                PathBuf::from(endpoint),
                self.root.output.clone(),
                Some(id.into()),
            )
            .await?,
        );
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
