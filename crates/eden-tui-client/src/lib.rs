//! Typed loopback transport shared by terminal frontends, without a dependency on the CLI.
use eden_protocol::{
    Event, Fault, Terminal,
    coding::{Block, QueueEntry, Record},
    presentation::{self, ActionRequest, ActivityTarget, Node},
    resources,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

#[derive(Serialize, Deserialize)]
struct Endpoint {
    address: String,
    token: String,
    session_id: u64,
}
fn fault(code: &str, message: impl Into<String>) -> Fault {
    Fault::new(code, "live-client", message)
}

/// Admission facts come from the host; cancellation only settles after `wait`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct State {
    #[serde(default)]
    pub session_id: u64,
    #[serde(default)]
    pub cwd: String,
    pub closed: bool,
    pub active_run: Option<u64>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub managing: bool,
    #[serde(default)]
    pub shell_runs: Vec<u64>,
    #[serde(default)]
    pub command_runs: Vec<u64>,
    #[serde(default)]
    pub pending_inputs: usize,
}
/// History contains committed public records; events are the retained live window, never synthetic messages.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Snapshot {
    #[serde(default)]
    pub reading: Option<eden_protocol::delivery::ReadingDocument>,
    #[serde(default)]
    pub diagnostic: Option<String>,
    pub presentation: presentation::Snapshot,
    pub state: State,
    pub history: Vec<Record>,
    pub events: Vec<Event>,
}
/// A missing request is not proof of rejection: the bounded host receipt cache may have expired.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum RequestStatus {
    Unknown,
    Running,
    Done { result: Result<Value, Fault> },
}
/// Queue kinds are explicit so steering cannot silently become a follow-up.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum QueueKind {
    Steering,
    FollowUp,
}
/// Delivery policy is applied by the same host queue transaction used by other frontends.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum QueueMode {
    #[serde(rename = "one")]
    OneAtATime,
    All,
}
/// A cloneable endpoint handle. Dropping it does not cancel or shut down its session.
#[derive(Clone, Debug)]
pub struct HostClient {
    endpoint: PathBuf,
    session_id: Option<u64>,
}
impl HostClient {
    /// Opening does not attach; callers explicitly own their attachment lifetime.
    pub fn new(endpoint: impl Into<PathBuf>) -> Self {
        Self {
            endpoint: endpoint.into(),
            session_id: None,
        }
    }
    /// Pin an explicitly attached Session. A replaced endpoint cannot redirect later mutations.
    pub fn for_session(endpoint: impl Into<PathBuf>, session_id: u64) -> Self {
        Self {
            endpoint: endpoint.into(),
            session_id: Some(session_id),
        }
    }
    async fn post<T: DeserializeOwned>(&self, route: &str, mut body: Value) -> Result<T, Fault> {
        if let Some(session_id) = self.session_id {
            if body.is_null() {
                body = json!({});
            }
            body["session_id"] = json!(session_id);
        }
        decode(call(&self.endpoint, "POST", route, Some(&body)).await?)
    }
    /// Attach activity ownership; detaching leaves accepted execution running.
    pub async fn attach(&self, frontend: &str) -> Result<u64, Fault> {
        let reply: Value = self
            .post("/attach", json!({ "frontend": frontend }))
            .await?;
        decode(reply["attachment"].clone())
    }
    /// End activity ownership without stopping a run.
    pub async fn detach(&self, attachment: u64) -> Result<(), Fault> {
        self.post::<Value>("/detach", json!({ "attachment": attachment }))
            .await?;
        Ok(())
    }
    /// Initial authoritative read, also suitable for reconnecting after an expired event cursor.
    pub async fn snapshot(&self) -> Result<Snapshot, Fault> {
        self.read_snapshot("/tui/snapshot").await
    }
    /// Wait for either presentation or model/tool events, refreshing the attachment heartbeat.
    /// `AttachmentExpired` means call `attach` again on this same endpoint and use its new lease;
    /// accepted work and request receipts still belong to the original Session.
    pub async fn poll_snapshot(
        &self,
        attachment: u64,
        presentation_sequence: u64,
        event_sequence: u64,
    ) -> Result<Snapshot, Fault> {
        self.read_snapshot(&format!(
            "/tui/snapshot?attachment={attachment}&after={presentation_sequence}&\
             events_after={event_sequence}"
        ))
        .await
    }
    async fn read_snapshot(&self, route: &str) -> Result<Snapshot, Fault> {
        let mut raw = call(&self.endpoint, "GET", route, None).await?;
        raw["presentation"] =
            serde_json::to_value(decode_presentation(raw["presentation"].take())?)
                .map_err(|e| fault("InvalidInput", e.to_string()))?;
        let snapshot: Snapshot = decode(raw)?;
        if self.session_id.is_some_and(|id| {
            id != snapshot.presentation.session_id || id != snapshot.state.session_id
        }) {
            return Err(fault(
                "SessionMismatch",
                "endpoint now serves another session",
            ));
        }
        Ok(snapshot)
    }
    /// Preserve text, images and references through the shared submission hook.
    pub async fn submit_blocks(&self, request_id: &str, content: Vec<Block>) -> Result<u64, Fault> {
        self.run(
            "/prompt",
            json!({ "request_id": request_id, "content": content }),
        )
        .await
    }
    async fn run(&self, route: &str, body: Value) -> Result<u64, Fault> {
        let reply: Value = self.post(route, body).await?;
        decode(reply["run_id"].clone())
    }
    /// Reconcile an uncertain submission before considering an explicit retry with the same identity.
    pub async fn request_status(&self, request_id: &str) -> Result<RequestStatus, Fault> {
        self.post("/request-status", json!({ "request_id": request_id }))
            .await
    }
    /// Cancellation acknowledgement does not imply process or persistence cleanup has finished.
    pub async fn cancel(&self, run_id: u64) -> Result<(), Fault> {
        self.post::<Value>("/cancel", json!({ "run_id": run_id }))
            .await?;
        Ok(())
    }
    /// The host returns only after run cleanup and committed terminal handling.
    pub async fn wait(&self, run_id: u64) -> Result<Terminal, Fault> {
        self.post("/terminal", json!({ "run_id": run_id })).await
    }
    /// Enqueue with an identity that survives a lost acknowledgement.
    pub async fn enqueue(
        &self,
        request_id: &str,
        kind: QueueKind,
        content: Vec<Block>,
    ) -> Result<QueueEntry, Fault> {
        self.post(
            "/enqueue",
            json!({ "request_id": request_id, "kind": kind, "content": content }),
        )
        .await
    }
    /// Read the shared effective request without sending a prompt or consuming temporary edits.
    pub async fn inspect_context(&self) -> Result<eden_protocol::context_edit::Snapshot, Fault> {
        self.post("/context/inspect", Value::Null).await
    }
    /// Apply the exact reviewed version through the host's idempotent receipt path.
    pub async fn edit_context(
        &self,
        request_id: &str,
        edit: eden_protocol::context_edit::Apply,
    ) -> Result<eden_protocol::context_edit::Snapshot, Fault> {
        self.post(
            "/context/apply",
            json!({ "request_id": request_id, "edit": edit }),
        )
        .await
    }
    /// Rebuild on a new branch; wait for the returned run before reading its committed view.
    pub async fn rebuild_context(
        &self,
        request_id: &str,
        rebuild: eden_protocol::context_edit::Rebuild,
    ) -> Result<u64, Fault> {
        let value: Value = self
            .post(
                "/context/rebuild",
                json!({ "request_id": request_id, "rebuild": rebuild }),
            )
            .await?;
        value["run_id"]
            .as_u64()
            .ok_or_else(|| fault("InvalidResponse", "missing rebuild run"))
    }
    /// Explicitly summarize effective context using the configured compaction policy.
    pub async fn compact(&self, request_id: &str, instructions: &str) -> Result<u64, Fault> {
        let value: Value = self
            .post(
                "/context/compact",
                json!({ "request_id": request_id, "instructions": instructions }),
            )
            .await?;
        value["run_id"]
            .as_u64()
            .ok_or_else(|| fault("InvalidResponse", "missing compaction run"))
    }
    /// Read saved-session choices without opening a source writer.
    pub async fn session_catalog(
        &self,
    ) -> Result<Vec<eden_protocol::session_reference::CatalogEntry>, Fault> {
        self.post("/session/catalog", Value::Null).await
    }
    /// List explicit source heads without changing the source branch.
    pub async fn session_branches(
        &self,
        path: &str,
    ) -> Result<Vec<eden_protocol::session_reference::Branch>, Fault> {
        self.post("/session/branches", json!({ "path": path }))
            .await
    }
    /// Capture an effective source preview; freezing selected entries is local and deterministic.
    pub async fn reference_preview(
        &self,
        path: &str,
        head: Option<u64>,
    ) -> Result<eden_protocol::session_reference::Preview, Fault> {
        self.post("/reference/preview", json!({ "path": path, "head": head }))
            .await
    }
    /// Send a self-contained reference snapshot with ordinary content.
    pub async fn submit_referenced(
        &self,
        request_id: &str,
        content: Vec<Block>,
        references: Vec<eden_protocol::session_reference::Reference>,
    ) -> Result<u64, Fault> {
        self.run(
            "/prompt",
            json!({ "request_id": request_id, "content": content, "references": references }),
        )
        .await
    }
    /// Preserve the reviewed image version decision through receipt recovery.
    pub async fn edit_images(
        &self,
        request_id: &str,
        edit: eden_protocol::context_edit::ImageEdit,
    ) -> Result<eden_protocol::context_edit::Snapshot, Fault> {
        self.post(
            "/context/images",
            json!({ "request_id": request_id, "edit": edit }),
        )
        .await
    }
    /// Inspect pending inputs on the active branch.
    pub async fn queued(&self) -> Result<Vec<QueueEntry>, Fault> {
        self.post("/queue/inspect", Value::Null).await
    }
    /// Withdraw only waiting entries; already delivered input remains owned by its run.
    pub async fn withdraw(
        &self,
        request_id: &str,
        ids: Option<Vec<u64>>,
    ) -> Result<Vec<QueueEntry>, Fault> {
        self.post(
            "/queue/withdraw",
            json!({ "request_id": request_id, "ids": ids }),
        )
        .await
    }
    /// Apply queue modes and return the run whose terminal confirms the change.
    pub async fn configure_queue(
        &self,
        request_id: &str,
        steering: QueueMode,
        follow_up: QueueMode,
    ) -> Result<u64, Fault> {
        self.run(
            "/queue/configure",
            json!({ "request_id": request_id, "steering": steering, "follow_up": follow_up }),
        )
        .await
    }
    /// An explicit shell has independent cancellation and may run alongside the model.
    pub async fn user_shell(
        &self,
        request_id: &str,
        command: &str,
        shell: &str,
        exclude_from_context: bool,
    ) -> Result<u64, Fault> {
        self.run(
            "/shell",
            json!({
                "request_id": request_id,
                "command": command,
                "shell": shell,
                "exclude_from_context": exclude_from_context,
            }),
        )
        .await
    }
    /// Cancel only the selected shell; use `wait` for the process cleanup barrier.
    pub async fn cancel_shell(&self, run_id: u64) -> Result<(), Fault> {
        self.post::<Value>("/shell/cancel", json!({ "run_id": run_id }))
            .await?;
        Ok(())
    }
    /// Read the same resource inventory used for the next prompt, including diagnostics.
    pub async fn resources(&self) -> Result<resources::Snapshot, Fault> {
        self.post("/resources", Value::Null).await
    }
    /// Inspect the effective model tool catalog after startup selections and exclusions.
    pub async fn tools(&self) -> Result<Vec<eden_protocol::coding::ToolDefinition>, Fault> {
        self.post("/tools", Value::Null).await
    }
    /// Discover installed contributed commands for completion without starting a run.
    pub async fn commands(&self) -> Result<resources::CommandCatalog, Fault> {
        self.post("/commands", Value::Null).await
    }
    /// Execute through the host command lifecycle and its normal terminal result.
    pub async fn command(
        &self,
        request_id: &str,
        name: &str,
        arguments: Value,
    ) -> Result<u64, Fault> {
        self.run(
            "/command",
            json!({ "request_id": request_id, "name": name, "arguments": arguments }),
        )
        .await
    }
    /// Configuration inspection retains the existing extensible management wire payload.
    pub async fn inspect_configuration(&self) -> Result<Value, Fault> {
        self.post("/configuration/inspect", Value::Null).await
    }
    /// Open the shared schema-derived configuration view.
    pub async fn open_configuration(
        &self,
        instance: &str,
    ) -> Result<presentation::Revision, Fault> {
        self.post("/configuration/open", json!({ "instance": instance }))
            .await
    }
    /// Public presentation actions retain their host revision and duplicate protection.
    pub async fn action(&self, request: ActionRequest) -> Result<Value, Fault> {
        self.post("/action", json!(request)).await
    }
    /// Private material is sent once and is never retained by this client. Retry by passing the same public request with empty inputs.
    pub async fn private_input(
        &self,
        input: eden_protocol::private_input::Submission,
    ) -> Result<Value, Fault> {
        self.post("/private-input", json!(input)).await
    }
    /// Report local editing without publishing draft contents.
    pub async fn activity(
        &self,
        attachment: u64,
        target: ActivityTarget,
        active: bool,
    ) -> Result<(), Fault> {
        self.post::<Value>(
            "/activity",
            json!({ "attachment": attachment, "target": target, "active": active }),
        )
        .await?;
        Ok(())
    }
    /// Resolve a host-owned interaction using its stable identity.
    pub async fn interaction(&self, interaction_id: u64, value: Value) -> Result<(), Fault> {
        self.post::<Value>(
            "/interaction",
            json!({ "interaction_id": interaction_id, "value": value }),
        )
        .await?;
        Ok(())
    }
}
fn decode<T: DeserializeOwned>(raw: Value) -> Result<T, Fault> {
    serde_json::from_value(raw).map_err(|e| fault("InvalidInput", e.to_string()))
}
/// Raw compatibility transport for existing adapters; typed consumers should use `HostClient`.
pub async fn call(
    endpoint_path: &Path,
    method: &str,
    route: &str,
    body: Option<&Value>,
) -> Result<Value, Fault> {
    call_with_deadlines(endpoint_path, method, route, body, Deadlines::default()).await
}
#[derive(Clone, Copy)]
struct Deadlines {
    connect: Duration,
    io: Duration,
}
impl Default for Deadlines {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(3),
            io: Duration::from_secs(10),
        }
    }
}
async fn call_with_deadlines(
    endpoint_path: &Path,
    method: &str,
    route: &str,
    body: Option<&Value>,
    deadlines: Deadlines,
) -> Result<Value, Fault> {
    let endpoint_bytes = tokio::time::timeout(deadlines.connect, tokio::fs::read(endpoint_path))
        .await
        .map_err(|_| fault("Unavailable", "endpoint read deadline exceeded"))?
        .map_err(|error| fault("FileFailure", error.to_string()))?;
    let endpoint: Endpoint = serde_json::from_slice(&endpoint_bytes)
        .map_err(|error| fault("InvalidInput", error.to_string()))?;
    let mut stream = tokio::time::timeout(deadlines.connect, TcpStream::connect(&endpoint.address))
        .await
        .map_err(|_| fault("Unavailable", "connection deadline exceeded"))?
        .map_err(|error| fault("Unavailable", error.to_string()))?;
    let bytes = body
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|error| fault("InvalidInput", error.to_string()))?
        .unwrap_or_default();
    let header = format!(
        "{method} {route} HTTP/1.1\r\nHost: {}\r\nX-Eden-Token: {}\r\nContent-Type: \
         application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        endpoint.address,
        endpoint.token,
        bytes.len()
    );
    tokio::time::timeout(deadlines.io, async {
        stream.write_all(header.as_bytes()).await?;
        stream.write_all(&bytes).await
    })
    .await
    .map_err(|_| {
        fault(
            "OutputFailure",
            "request write deadline exceeded; acceptance unknown",
        )
    })?
    .map_err(|error| fault("OutputFailure", error.to_string()))?;
    // No background task owns this socket: dropping a cancelled wait closes the transport,
    // while execution remains owned by the host. Only terminal observation is unbounded.
    if method == "POST"
        && matches!(
            route.split('?').next(),
            Some("/terminal" | "/configuration/wait")
        )
    {
        read_response(&mut stream).await
    } else {
        tokio::time::timeout(deadlines.io, read_response(&mut stream))
            .await
            .map_err(|_| {
                fault(
                    "InputFailure",
                    "response deadline exceeded; mutation acceptance unknown, reconcile the \
                     request identity before retrying",
                )
            })?
    }
}
async fn read_response(stream: &mut TcpStream) -> Result<Value, Fault> {
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .map_err(|error| fault("InputFailure", error.to_string()))?;
    let start = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| fault("InputFailure", "invalid response; acceptance unknown"))?
        + 4;
    let envelope: Value = serde_json::from_slice(&response[start..])
        .map_err(|error| fault("InputFailure", error.to_string()))?;
    if envelope["ok"] == true {
        Ok(envelope["result"].clone())
    } else {
        Err(serde_json::from_value(envelope["error"].clone())
            .map_err(|error| fault("InputFailure", error.to_string()))?)
    }
}
/// Decode additive presentation nodes without enabling actions from an unknown protocol version.
pub fn decode_presentation(mut value: Value) -> Result<presentation::Snapshot, Fault> {
    let compatible = value["version"] == eden_protocol::presentation::VERSION;
    if let Some(views) = value["views"].as_array_mut() {
        for view in views {
            let known_slot =
                serde_json::from_value::<eden_protocol::presentation::Slot>(view["slot"].clone())
                    .is_ok();
            let mut nodes = if compatible && known_slot {
                view["nodes"]
                    .as_array()
                    .map(|nodes| nodes.iter().flat_map(compatible_nodes).collect::<Vec<_>>())
                    .unwrap_or_default()
            } else {
                view["active"] = json!(false);
                vec![]
            };
            if nodes.is_empty() {
                nodes.push(json!({
                    "kind": "text",
                    "id": "compatibility-fallback",
                    "text": view["fallback"]
                        .as_str()
                        .unwrap_or("Unsupported presentation"),
                }));
            }
            if !known_slot {
                view["slot"] = json!("panel");
            }
            view["nodes"] = Value::Array(nodes);
        }
    }
    decode(value)
}
fn compatible_nodes(raw: &Value) -> Vec<Value> {
    let mut node = raw.clone();
    let children = raw["children"]
        .as_array()
        .map(|items| items.iter().flat_map(compatible_nodes).collect::<Vec<_>>())
        .unwrap_or_default();
    if raw["kind"] == "group" {
        node["children"] = json!(children);
    }
    if serde_json::from_value::<Node>(node.clone()).is_ok() {
        vec![node]
    } else {
        let mut fallback = vec![json!({
            "kind": "text",
            "id": raw["id"].as_str().unwrap_or("unknown-node"),
            "text": raw["fallback"]
                .as_str()
                .unwrap_or("Unsupported presentation node"),
        })];
        fallback.extend(children);
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_presentation_is_readable_without_enabling_future_actions() {
        let raw = json!({
            "version": 999,
            "session_id": "7",
            "sequence": 1,
            "activity": [],
            "pending_interactions": [],
            "views": [{
                "owner": "extension",
                "run_id": 1,
                "revision": 1,
                "active": true,
                "id": "future",
                "slot": "future",
                "title": "Settings",
                "fallback": "Please update",
                "source": null,
                "platforms": [],
                "nodes": [{ "kind": "button", "id": "send", "label": "Send", "action": "unsafe" }],
            }],
        });
        let snapshot = decode_presentation(raw).unwrap();
        assert!(!snapshot.views[0].active);
        assert!(
            matches!(&snapshot.views[0].view.nodes[0], Node::Text { text, .. } if text == "Please update")
        );
    }

    #[test]
    fn queue_modes_use_existing_host_wire_values() {
        assert_eq!(json!(QueueMode::OneAtATime), json!("one"));
        assert_eq!(json!(QueueMode::All), json!("all"));
    }

    #[test]
    fn receipt_keeps_failure_distinct_from_unknown_acceptance() {
        let result: RequestStatus = decode(json!({
            "status": "done",
            "result": { "Err": { "code": "Unavailable", "source": "session", "message": "busy" } },
        }))
        .unwrap();
        assert!(
            matches!(result, RequestStatus::Done { result: Err(error) } if error.code == "Unavailable")
        );
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn silent_host() -> (
        PathBuf,
        tokio::task::JoinHandle<Vec<u8>>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let path = std::env::temp_dir().join(format!(
            "eden-client-deadline-{}-{}.json",
            std::process::id(),
            address.port()
        ));
        tokio::fs::write(
            &path,
            serde_json::to_vec(&Endpoint {
                address: address.to_string(),
                token: "test-token".into(),
                session_id: 1,
            })
            .unwrap(),
        )
        .await
        .unwrap();
        let (ready, connected) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut first = [0; 4096];
            let count = socket.read(&mut first).await.unwrap();
            assert!(count > 0);
            let mut received = first[..count].to_vec();
            let _ = ready.send(());
            // EOF is the cleanup evidence: no response is sent and only client cancellation closes it.
            socket.read_to_end(&mut received).await.unwrap();
            received
        });
        (path, server, connected)
    }
    fn short_deadlines() -> Deadlines {
        Deadlines {
            connect: Duration::from_secs(1),
            io: Duration::from_millis(20),
        }
    }

    #[tokio::test]
    async fn response_deadlines_release_socket_and_leave_mutation_acceptance_unknown() {
        for (method, route, body) in [
            ("GET", "/tui/snapshot", None),
            (
                "POST",
                "/prompt",
                Some(json!({ "request_id": "stable", "text": "once" })),
            ),
            (
                "POST",
                "/request-status",
                Some(json!({ "request_id": "stable" })),
            ),
        ] {
            let (path, server, _ready) = silent_host().await;
            let error = call_with_deadlines(&path, method, route, body.as_ref(), short_deadlines())
                .await
                .unwrap_err();
            assert_eq!(error.code, "InputFailure");
            assert!(error.message.contains("acceptance unknown"));
            let request = tokio::time::timeout(Duration::from_secs(1), server)
                .await
                .unwrap()
                .unwrap();
            assert!(
                String::from_utf8(request)
                    .unwrap()
                    .starts_with(&format!("{method} {route} HTTP/1.1"))
            );
            tokio::fs::remove_file(path).await.unwrap();
        }
    }
    #[tokio::test]
    async fn terminal_wait_outlives_response_deadline_but_dropping_it_closes_socket() {
        let (path, server, ready) = silent_host().await;
        let body = json!({ "run_id": 1 });
        let mut waiting = Box::pin(call_with_deadlines(
            &path,
            "POST",
            "/terminal",
            Some(&body),
            short_deadlines(),
        ));
        tokio::select! {
            result = ready => result.unwrap(),
            result = &mut waiting =>
                panic!("terminal wait completed before peer observation: {result:?}"),
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(80), waiting)
                .await
                .is_err()
        );
        let request = tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
        assert!(
            String::from_utf8(request)
                .unwrap()
                .starts_with("POST /terminal HTTP/1.1")
        );
        tokio::fs::remove_file(path).await.unwrap();
    }
}
