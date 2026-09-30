//! ACP transport for the Grok-derived frontend, backed by the existing Eden host.
mod forms;
mod models;
mod projection;
mod resources;
mod sessions;
mod submission;

use eden_protocol::Fault;
use eden_tui_client::{HostClient, Snapshot};
use projection::{Projection, text};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, Notify, mpsc},
};

struct View {
    snapshot: Snapshot,
    projection: Projection,
    prompts: BTreeMap<u64, (String, Option<String>)>,
    context_revision: Option<(u64, Option<u64>)>,
}
struct Adapter {
    client: HostClient,
    reading_diagnostic: Option<String>,
    owns_reader: bool,
    stopped: std::sync::atomic::AtomicBool,
    endpoint: PathBuf,
    session: u64,
    identity: String,
    following: std::sync::atomic::AtomicBool,
    load_pending: std::sync::atomic::AtomicBool,
    loaded: Notify,
    lease: AtomicU64,
    shell_run: AtomicU64,
    resource_revision: AtomicU64,
    model_operation: Mutex<()>,
    active_prompt: Mutex<Option<submission::Prompt>>,
    nonce: u128,
    next: AtomicU64,
    view: Mutex<View>,
    output: mpsc::UnboundedSender<Value>,
}

fn fault(message: impl Into<String>) -> Fault {
    Fault::new("InvalidInput", "grok-adapter", message)
}

impl Adapter {
    async fn new(
        endpoint: PathBuf,
        output: mpsc::UnboundedSender<Value>,
        identity: Option<String>,
    ) -> Result<Self, Fault> {
        let metadata: Value = serde_json::from_slice(
            &tokio::fs::read(&endpoint)
                .await
                .map_err(|error| fault(error.to_string()))?,
        )
        .map_err(|error| fault(error.to_string()))?;
        let expected = metadata["session_id"]
            .as_u64()
            .ok_or_else(|| fault("Missing endpoint Session identity"))?;
        let snapshot = HostClient::for_session(&endpoint, expected)
            .snapshot()
            .await?;
        let session = snapshot.presentation.session_id;
        Ok(Self {
            client: HostClient::for_session(&endpoint, session),
            endpoint,
            reading_diagnostic: None,
            owns_reader: false,
            stopped: std::sync::atomic::AtomicBool::new(false),
            session,
            identity: identity.unwrap_or_else(|| format!("eden-{session}")),
            following: std::sync::atomic::AtomicBool::new(false),
            load_pending: std::sync::atomic::AtomicBool::new(false),
            loaded: Notify::new(),
            shell_run: AtomicU64::new(0),
            resource_revision: AtomicU64::new(0),
            model_operation: Mutex::new(()),
            active_prompt: Mutex::new(None),
            lease: AtomicU64::new(0),
            nonce: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| fault(e.to_string()))?
                .as_nanos(),
            next: AtomicU64::new(0),
            view: Mutex::new(View {
                snapshot,
                projection: Projection::default(),
                prompts: BTreeMap::new(),
                context_revision: None,
            }),
            output,
        })
    }

    fn request_id(&self) -> String {
        format!(
            "grok-{}-{}",
            self.nonce,
            self.next.fetch_add(1, Ordering::Relaxed)
        )
    }
    fn send(&self, mut value: Value) {
        value["jsonrpc"] = json!("2.0");
        let _ = self.output.send(value);
    }
    fn update(&self, update: Value, replay: bool) {
        self.correlated_update(update, json!({ "isReplay": replay }));
    }
    fn correlated_update(&self, update: Value, meta: Value) {
        self.send(json!({
            "method": "session/update",
            "params": { "sessionId": self.identity, "update": update, "_meta": meta },
        }));
    }
    async fn post(&self, route: &str, mut body: Value) -> Result<Value, Fault> {
        body["session_id"] = json!(self.session);
        eden_tui_client::call(&self.endpoint, "POST", route, Some(&body)).await
    }
    async fn complete(&self, run: u64) -> Result<Value, Fault> {
        self.client.wait(run).await?.into_result()
    }
    async fn project(&self, snapshot: Snapshot, replay: bool) {
        let resources = snapshot
            .events
            .iter()
            .rev()
            .filter(|event| event.kind == "settled")
            .find_map(|event| {
                serde_json::from_value::<eden_protocol::resources::ResourceReply>(
                    event.payload["outcome"]["value"].clone(),
                )
                .ok()
            });
        let mut view = self.view.lock().await;
        if replay {
            view.projection = Projection::default();
        } else if self.load_pending.load(Ordering::Acquire) {
            return;
        }
        let mut model_changed = false;
        for mut update in view.projection.apply(&snapshot, replay) {
            let run = update.as_object_mut().and_then(|u| u.remove("_eden_run"));
            let attempt = update
                .as_object_mut()
                .and_then(|u| u.remove("_eden_attempt"));
            let mut meta =
                json!({ "isReplay": replay, "edenRunId": run, "edenAttemptId": attempt });
            if !replay
                && let Some((prompt, request)) = run
                    .as_ref()
                    .and_then(Value::as_u64)
                    .and_then(|run| view.prompts.get(&run))
            {
                meta["promptId"] = json!(prompt);
                meta["edenRequestId"] = json!(request);
            }
            if let Some(title) = update.get("_eden_title") {
                self.send(json!({
                    "method": "_eden/session/title",
                    "params": { "sessionId": self.identity, "title": title },
                }));
            } else if update.get("_eden_model").is_some() {
                model_changed = true;
            } else {
                self.correlated_update(update, meta);
            }
        }
        let context_revision = (
            snapshot.history.last().map_or(0, |r| r.sequence),
            snapshot.state.active_run,
        );
        let context_changed = replay || view.context_revision != Some(context_revision);
        view.context_revision = Some(context_revision);
        let context_readable = !snapshot.state.read_only && snapshot.state.active_run.is_none();
        view.snapshot = snapshot;
        drop(view);
        if context_changed {
            let context = if context_readable {
                self.client.inspect_context().await.ok()
            } else {
                None
            };
            if self.view.lock().await.context_revision != Some(context_revision) {
                return;
            }
            self.send(json!({
                "method": "_eden/context/state",
                "params": {
                    "sessionId": self.identity,
                    "used": context.as_ref().map(|c| c.estimated_tokens()),
                    "window": context
                        .as_ref()
                        .and_then(|c| c.model.as_ref())
                        .map(|m| m.limits.context_window),
                    "estimated": context.is_some(),
                },
            }));
        }
        if let Some(resources) = resources {
            self.publish_resources(&resources.snapshot, false);
        }
        if model_changed && let Ok(models) = self.model_state().await {
            self.send(json!({
                "method": "_eden/model/state",
                "params": { "sessionId": self.identity, "models": models },
            }));
        }
    }
    async fn follow(self: Arc<Self>) {
        let mut disconnected = false;
        loop {
            if self.stopped.load(Ordering::Acquire) {
                return;
            }
            let loaded = self.loaded.notified();
            if self.load_pending.load(Ordering::Acquire) {
                loaded.await;
                continue;
            }
            let previous = self.view.lock().await.snapshot.clone();
            let lease = self.lease.load(Ordering::Relaxed);
            if lease == 0 {
                return;
            }
            match self.client.poll_incremental(lease, previous).await {
                Ok(snapshot) => {
                    if disconnected {
                        self.message("Reconnected to the same Eden Session.");
                    }
                    disconnected = false;
                    self.project(snapshot, false).await;
                    self.finish_adopted().await;
                }
                Err(error)
                    if error.code == "AttachmentExpired"
                        || (error.code == "InvalidInput"
                            && error.message.contains("attachment")) =>
                {
                    match self.client.attach("tui").await {
                        Ok(lease) => {
                            self.lease.store(lease, Ordering::Relaxed);
                        }
                        Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
                    }
                }
                Err(error) => {
                    if !disconnected {
                        self.message(&format!("Host disconnected: {error}"));
                    }
                    if matches!(error.code.as_str(), "SessionMismatch" | "SessionClosed") {
                        return;
                    }
                    disconnected = true;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
    fn message(&self, body: &str) {
        self.update(
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": body },
            }),
            false,
        );
    }
    async fn prompt(&self, params: &Value) -> Result<Value, Fault> {
        let blocks = params["prompt"]
            .as_array()
            .ok_or_else(|| fault("Missing prompt blocks"))?;
        let body = blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if body == "/eden-status" {
            self.message(
                &serde_json::to_string_pretty(&self.client.snapshot().await?.state)
                    .map_err(|e| fault(e.to_string()))?,
            );
            return Ok(json!({ "stopReason": "end_turn" }));
        }
        if body == "/usage" {
            self.message(&projection::usage_text(
                &self.client.snapshot().await?.history,
            ));
            return Ok(json!({ "stopReason": "end_turn" }));
        }
        if body == "/capabilities" {
            self.message(
                "Connected: conversation, tools and Diff, cancellation, history replay, model \
                 selection, use & save model, local display settings, saved-session picker, shell \
                 mode, usage, authentication, Session configuration, skills/templates and \
                 resource reload.\nStill being connected: attachments/images, queues, context \
                 editing, plugins, delivery and native UI replacement.\nNo bundled backend: \
                 subagents, voice, scheduled user tasks and xAI cloud services.",
            );
            return Ok(json!({ "stopReason": "end_turn" }));
        }
        if matches!(
            body.split_whitespace().next(),
            Some("/voice" | "/loop" | "/tasks" | "/imagine" | "/imagine-video")
        ) {
            self.message(
                "This capability is not provided by the bundled Eden backend. Use /capabilities \
                 to see available features.",
            );
            return Ok(json!({ "stopReason": "end_turn" }));
        }
        self.run_prompt(params, &body, blocks).await
    }
    async fn dispatch(self: &Arc<Self>, request: &Value) -> Result<Value, Fault> {
        let method = text(&request["method"]).trim_start_matches('_');
        let params = &request["params"];
        if let Some(session) = params["sessionId"].as_str()
            && session != self.identity
        {
            return Err(fault("Session identity mismatch"));
        }
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": 1,
                "agentCapabilities": { "loadSession": true },
                "agentInfo": {
                    "name": "eden",
                    "title": "Eden",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "authMethods": [{ "id": "eden-host", "name": "Eden host attachment" }],
                "_meta": {
                    "grokShell": false,
                    "cancelRewind": false,
                    "modelState": self.model_state().await?,
                    "availableCommands": self.available_commands().await?,
                },
            })),
            "authenticate" => {
                self.client.snapshot().await?;
                Ok(json!({}))
            }
            "session/load" | "session/new" => {
                self.load_pending.store(true, Ordering::Release);
                if self.lease.load(Ordering::Relaxed) == 0 {
                    let lease = self.client.attach("tui").await?;
                    self.lease.store(lease, Ordering::Relaxed);
                }
                let snapshot = self.client.snapshot().await?;
                let running = self.adopt_run(&snapshot).await;
                self.project(snapshot, true).await;
                self.publish_resources(&self.resource_inventory().await?, true);
                if self.view.lock().await.snapshot.state.read_only {
                    self.update(
                        json!({
                            "sessionUpdate": "agent_message_chunk",
                            "content": {
                                "type": "text",
                                "text": format!(
                                    "\n\nRead-only history. {}\nUse /sessions to preview a \
                                     migrated copy with the current composition. The original \
                                     remains unchanged.",
                                    self.reading_diagnostic
                                        .as_deref()
                                        .unwrap_or("Execution and edits are disabled.")
                                ),
                            },
                        }),
                        true,
                    );
                }
                let models = self.model_state().await?;
                if let Some(error) = models["_meta"]["edenDiagnostic"].as_str() {
                    self.update(
                        json!({
                            "sessionUpdate": "agent_message_chunk",
                            "content": {
                                "type": "text",
                                "text": format!(
                                    "{error}\nSelect a model with /model, authenticate with \
                                     /auth, or inspect /config."
                                ),
                            },
                        }),
                        true,
                    );
                }
                let reply = json!({
                    "sessionId": self.identity,
                    "models": models,
                    "_meta": { "x.ai/runningPromptId": running },
                });
                Ok(reply)
            }
            "x.ai/compact_conversation" => {
                let run = self.client.compact(&self.request_id(), "").await?;
                self.complete(run).await?;
                self.project(self.client.snapshot().await?, false).await;
                Ok(json!({}))
            }
            "x.ai/prompt_history" => {
                let snapshot = self.client.snapshot().await?;
                let prompts: Vec<_> = snapshot
                    .history
                    .iter()
                    .filter(|record| {
                        record.payload["type"] == "message" && record.payload["role"] == "user"
                    })
                    .map(|record| {
                        record.payload["content"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|block| block["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .collect();
                Ok(json!({ "prompts": prompts }))
            }
            "session/prompt" => {
                if self.view.lock().await.snapshot.state.read_only {
                    return Err(fault(
                        "This history is read-only; use /sessions to resume a matching session or \
                         create a migrated copy",
                    ));
                }
                self.prompt(params).await
            }
            "x.ai/session/rename" => {
                let reply = self
                    .post(
                        "/manage/metadata",
                        json!({
                            "request_id": self.request_id(),
                            "name": params["title"],
                            "preserve_tags": true,
                            "tags": [],
                        }),
                    )
                    .await?;
                self.complete(
                    reply["run_id"]
                        .as_u64()
                        .ok_or_else(|| fault("Missing rename operation"))?,
                )
                .await?;
                Ok(json!({}))
            }
            "session/cancel" => self.cancel_prompt(params).await,
            "session/set_model" => {
                let _operation = self.model_operation.lock().await;
                let selection = self.model_selection(params).await?;
                let reply = self
                    .post(
                        "/models/select",
                        json!({ "request_id": self.request_id(), "selection": selection }),
                    )
                    .await?;
                self.complete(
                    reply["run_id"]
                        .as_u64()
                        .ok_or_else(|| fault("Missing model operation"))?,
                )
                .await?;
                self.project(self.client.snapshot().await?, false).await;
                Ok(json!({}))
            }
            "eden/model/default" => {
                let _operation = self.model_operation.lock().await;
                let selection = self.model_selection(params).await?;
                let provider = text(&selection["provider"]);
                let model = text(&selection["model"]);
                let chosen = self
                    .post(
                        "/models/select",
                        json!({ "request_id": self.request_id(), "selection": selection }),
                    )
                    .await?;
                self.complete(
                    chosen["run_id"]
                        .as_u64()
                        .ok_or_else(|| fault("Missing selection operation"))?,
                )
                .await?;
                self.project(self.client.snapshot().await?, false).await;
                let saved = self
                    .post(
                        "/models/catalog",
                        json!({
                            "request_id": self.request_id(),
                            "request": { "action": "set_default", "selection": selection },
                        }),
                    )
                    .await;
                let result = match saved {
                    Ok(reply) => {
                        self.complete(
                            reply["run_id"]
                                .as_u64()
                                .ok_or_else(|| fault("Missing catalog operation"))?,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                result.map_err(|error| {
                    fault(format!(
                        "Current session changed to {provider}/{model}; saving the new-session \
                         default failed: {error}"
                    ))
                })?;
                Ok(json!({}))
            }
            _ => Err(fault(format!("Method not connected: {method}"))),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = PathBuf::from(
        std::env::var_os("EDEN_GROK_ENDPOINT").ok_or("EDEN_GROK_ENDPOINT is required")?,
    );
    let (output, mut responses) = mpsc::unbounded_channel::<Value>();
    let adapter = Arc::new(Adapter::new(endpoint, output, None).await?);
    if std::env::args().any(|argument| argument == "--check") {
        adapter.available_commands().await?;
        return Ok(());
    }
    let server = Arc::new(sessions::Server::new(adapter));
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(value) = responses.recv().await {
            let mut line = serde_json::to_vec(&value)?;
            line.push(b'\n');
            stdout.write_all(&line).await?;
            stdout.flush().await?;
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });
    let mut requests = BufReader::new(tokio::io::stdin()).lines();
    let mut tasks = tokio::task::JoinSet::new();
    while let Some(line) = requests.next_line().await? {
        let request: Value = serde_json::from_str(&line)?;
        let server = server.clone();
        tasks.spawn(async move {
            let result = if text(&request["method"]).trim_start_matches('_') == "eden/ui" {
                server.forms.dispatch(&server, &request["params"]).await
            } else if text(&request["method"]).trim_start_matches('_') == "x.ai/session/list" {
                server.list(&request["params"]).await
            } else {
                match server.target(&request).await {
                    Ok(target) => target.dispatch(&request).await,
                    Err(error) => Err(error),
                }
            };
            let adapter = server.root().await;
            if !request["id"].is_null() {
                match result {
                    Ok(value) => adapter.send(json!({ "id": request["id"], "result": value })),
                    Err(error) => adapter.send(json!({
                        "id": request["id"],
                        "error": { "code": -32603, "message": error.to_string() },
                    })),
                }
            }
            if matches!(text(&request["method"]), "session/load" | "session/new") {
                server.finish_load(&request["params"]).await;
            }
            server.follow_all().await;
        });
        while tasks.try_join_next().is_some() {}
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    server.stop_followers().await;
    server.forms.close_all(&server).await;
    for adapter in server.all().await {
        let lease = adapter.lease.load(Ordering::Relaxed);
        if lease != 0 && adapter.endpoint.exists() {
            adapter.client.detach(lease).await?;
        }
        if adapter.owns_reader && adapter.endpoint.exists() {
            let _ = adapter.post("/shutdown", json!({})).await;
        }
    }
    drop(server);
    writer
        .await?
        .map_err(|e| -> Box<dyn std::error::Error> { e })?;
    Ok(())
}
