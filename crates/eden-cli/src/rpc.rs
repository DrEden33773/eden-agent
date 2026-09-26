//! Versioned stdio RPC keeps connection ownership separate from Session operations.
use crate::cli::Cli;
use eden_agent::{Fault, Session, SessionOptions};
use eden_protocol::coding::Block;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    io::{Read, Write},
    path::PathBuf,
    pin::Pin,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::mpsc,
    task::JoinSet,
};

const METHODS: &[&str] = &[
    "ping",
    "capabilities",
    "shutdown",
    "session.new",
    "session.open",
    "state",
    "config.inspect",
    "config.validate",
    "config.preview",
    "config.apply",
    "config.status",
    "prompt",
    "resume",
    "cancel",
    "history",
    "export",
    "share",
    "preview.forget",
    "update",
    "tree",
    "queue",
    "enqueue",
    "steer",
    "follow_up",
    "queue.withdraw",
    "queue.configure",
    "control",
    "tools",
    "resources",
    "resources.reload",
    "commands",
    "command",
    "metadata",
    "navigate",
    "compact",
    "attachment.include",
    "models",
    "model.current",
    "model.select",
    "model.catalog",
    "router.list",
    "router.request",
    "auth.request",
    "auth.status",
    "auth.input",
    "interaction.respond",
    "session.copy",
    "shell",
    "shell.cancel",
];

#[derive(Clone, Copy)]
struct Limits {
    frame: usize,
    output: usize,
    pending: usize,
    write_timeout: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            frame: 1024 * 1024,
            output: 256,
            pending: 64,
            write_timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u64,
    id: String,
    session_id: Option<u64>,
    method: String,
    #[serde(default)]
    params: Value,
}

fn fault(code: &str, message: impl Into<String>) -> Fault {
    Fault::new(code, "rpc", message)
}
fn parameter<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, Fault> {
    serde_json::from_value(value).map_err(|error| fault("InvalidParams", error.to_string()))
}
fn field<T: serde::de::DeserializeOwned>(request: &Request, key: &str) -> Result<T, Fault> {
    parameter(request.params.get(key).cloned().unwrap_or(Value::Null))
}
fn content(request: &Request) -> Result<Vec<Block>, Fault> {
    if request.params.get("content").is_some() {
        field(request, "content")
    } else {
        Ok(vec![Block::Text {
            text: field(request, "text")?,
        }])
    }
}
fn response(kind: &str, id: Option<&str>, session: u64, data: Value) -> Value {
    let mut value = json!({ "version": 1, "type": kind, "id": id, "session_id": session });
    if let (Some(target), Some(fields)) = (value.as_object_mut(), data.as_object()) {
        target.extend(fields.clone());
    }
    value
}
fn error(id: Option<&str>, session: u64, error: Fault) -> Value {
    response("error", id, session, json!({ "error": error }))
}
fn send(output: &mpsc::Sender<Value>, value: Value) -> Result<(), Fault> {
    output
        .try_send(value)
        .map_err(|_| fault("OutputUnavailable", "output queue is full or closed"))
}

struct Frames<R> {
    reader: BufReader<R>,
    buffer: Vec<u8>,
    max: usize,
}
impl<R: AsyncRead + Unpin> Frames<R> {
    fn new(reader: R, max: usize) -> Self {
        Self {
            reader: BufReader::new(reader),
            buffer: Vec::new(),
            max,
        }
    }
    async fn next(&mut self) -> Result<Option<Vec<u8>>, Fault> {
        loop {
            let bytes = self
                .reader
                .fill_buf()
                .await
                .map_err(|e| fault("InputFailure", e.to_string()))?;
            if bytes.is_empty() {
                return Ok((!self.buffer.is_empty()).then(|| std::mem::take(&mut self.buffer)));
            }
            let end = bytes.iter().position(|byte| *byte == b'\n');
            let length = end.map_or(bytes.len(), |index| index + 1);
            if self.buffer.len() + length > self.max {
                return Err(fault("FrameTooLarge", "request exceeds the frame limit"));
            }
            self.buffer.extend_from_slice(&bytes[..length]);
            self.reader.consume(length);
            if end.is_some() {
                self.buffer.pop();
                if self.buffer.last() == Some(&b'\r') {
                    self.buffer.pop();
                }
                return Ok(Some(std::mem::take(&mut self.buffer)));
            }
        }
    }
}

fn parse(bytes: &[u8]) -> Result<Request, (Option<String>, Fault)> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| {
        (
            None,
            fault("InvalidRequest", "request must be valid UTF-8 JSON"),
        )
    })?;
    let id = value.get("id").and_then(Value::as_str).map(str::to_owned);
    let request: Request = serde_json::from_value(value).map_err(|_| {
        (
            id.clone(),
            fault(
                "InvalidRequest",
                "expected version, string id, method, optional session_id and params",
            ),
        )
    })?;
    if request.version != 1 {
        return Err((
            id,
            fault("UnsupportedVersion", "only protocol version 1 is supported"),
        ));
    }
    if request.id.is_empty() || request.id.len() > 256 {
        return Err((
            None,
            fault("InvalidRequest", "id must contain 1 to 256 UTF-8 bytes"),
        ));
    }
    if !request.params.is_null() && !request.params.is_object() {
        return Err((id, fault("InvalidParams", "params must be an object")));
    }
    Ok(request)
}

type Query = Pin<Box<dyn Future<Output = Result<Value, Fault>> + Send>>;
enum Dispatch {
    Immediate(Value),
    Run(u64, bool),
    Query(Query),
    Management(Query),
}
fn query(future: impl Future<Output = Result<Value, Fault>> + Send + 'static) -> Dispatch {
    Dispatch::Query(Box::pin(future))
}
fn dispatch(
    session: &Session,
    request: &Request,
    composition: &std::path::Path,
) -> Result<Dispatch, Fault> {
    let s = session.clone();
    let result = match request.method.as_str() {
        "ping" => Dispatch::Immediate(json!({ "pong": true })),
        "capabilities" => {
            let enabled = request
                .params
                .get("interactions")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            session.set_interactions(enabled);
            Dispatch::Immediate(json!({ "interactions": enabled, "methods": METHODS }))
        }
        "state" => Dispatch::Immediate(json!(session.state())),
        "config.inspect" => query(async move { Ok(json!(s.inspect_configuration().await?)) }),
        "config.validate" => {
            let change: eden_agent::configuration::Change = parameter(request.params.clone())?;
            query(async move { Ok(json!(s.validate_configuration(change).await?)) })
        }
        "config.preview" => {
            let change: eden_agent::configuration::Change = parameter(request.params.clone())?;
            query(async move { Ok(json!(s.preview_configuration(change).await?)) })
        }
        "config.apply" => {
            let change: eden_agent::configuration::Change = parameter(request.params.clone())?;
            let mode: eden_agent::configuration::ApplyMode = request
                .params
                .get("mode")
                .cloned()
                .map(parameter)
                .transpose()?
                .unwrap_or_default();
            Dispatch::Management(Box::pin(async move {
                Ok(json!({ "operation": s.apply_configuration(change, mode).await? }))
            }))
        }
        "config.status" => Dispatch::Immediate(json!(
            session.configuration_operation(field(request, "operation")?)?
        )),
        "prompt" => Dispatch::Run(session.submit_blocks(content(request)?)?, false),
        "resume" => Dispatch::Run(session.resume()?, false),
        "shell" => Dispatch::Run(
            session.user_shell(
                field(request, "command")?,
                field(request, "shell")?,
                request
                    .params
                    .get("exclude_from_context")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            )?,
            false,
        ),
        "shell.cancel" => {
            session.cancel_shell(field(request, "run_id")?)?;
            Dispatch::Immediate(json!({ "cancel_requested": true }))
        }
        "cancel" => {
            session.cancel(field(request, "run_id")?)?;
            Dispatch::Immediate(json!({ "cancel_requested": true }))
        }
        "export" => {
            let selection = parameter(
                request
                    .params
                    .get("selection")
                    .cloned()
                    .unwrap_or(json!({})),
            )?;
            let format = parameter(
                request
                    .params
                    .get("format")
                    .cloned()
                    .unwrap_or(json!("jsonl")),
            )?;
            query(async move {
                let (id, artifact) = s.prepare_export(selection, format).await?;
                let mut value = json!(artifact);
                value["preview_id"] = json!(id);
                Ok(value)
            })
        }
        "share" => {
            let run = if let Some(id) = request.params.get("preview_id").and_then(Value::as_str) {
                session.publish_preview(id, field(request, "confirmed")?)?
            } else {
                session.publish(parameter(request.params.clone())?)?
            };
            Dispatch::Run(run, false)
        }
        "preview.forget" => Dispatch::Immediate(json!({
            "forgotten":
                session.forget_preview(&field::<String>(request, "preview_id")?),
        })),
        "update" => Dispatch::Run(session.update(parameter(request.params.clone())?)?, false),
        "history" => query(async move { Ok(json!(s.history().await?)) }),
        "tree" => query(async move {
            let records = s.history().await?;
            let (head, branch) = eden_protocol::history::branch_state(&records)?;
            Ok(json!({ "head": head, "branch": branch, "records": records }))
        }),
        "queue" => query(async move { Ok(json!(s.queued().await?)) }),
        "enqueue" | "steer" | "follow_up" => {
            let kind = match request.method.as_str() {
                "steer" => "steering".to_owned(),
                "follow_up" => "follow_up".to_owned(),
                _ => field(request, "kind")?,
            };
            let content = content(request)?;
            query(async move { Ok(json!(s.enqueue(&kind, content).await?)) })
        }
        "queue.withdraw" => {
            let ids = field(request, "ids")?;
            query(async move { Ok(json!(s.withdraw_queue(ids).await?)) })
        }
        "queue.configure" => Dispatch::Run(
            session.configure_queue(field(request, "steering")?, field(request, "follow_up")?)?,
            false,
        ),
        "control" => {
            let control = parameter(request.params.clone())?;
            query(async move { Ok(json!(s.control(control).await?)) })
        }
        "tools" => query(async move { Ok(json!(s.tools().await?)) }),
        "resources" => query(async move { Ok(json!(s.resources().await?)) }),
        "resources.reload" => Dispatch::Run(session.reload_resources()?, false),
        "commands" => query(async move { Ok(json!(s.commands().await?)) }),
        "command" => Dispatch::Run(
            session.command(
                field(request, "name")?,
                request
                    .params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(json!({})),
            )?,
            false,
        ),
        "metadata" => Dispatch::Run(
            session.set_metadata(
                field(request, "name")?,
                request
                    .params
                    .get("tags")
                    .cloned()
                    .map(parameter)
                    .transpose()?
                    .unwrap_or_default(),
            )?,
            false,
        ),
        "navigate" => Dispatch::Run(
            session.navigate(
                field(request, "target")?,
                field(request, "branch")?,
                request
                    .params
                    .get("summarize")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            )?,
            false,
        ),
        "compact" => Dispatch::Run(
            session.compact(
                request
                    .params
                    .get("instructions")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
            )?,
            false,
        ),
        "attachment.include" => Dispatch::Run(
            session.include_attachment(field(request, "record_id")?)?,
            false,
        ),
        "models" => query(async move { Ok(json!(s.models().await?)) }),
        "model.current" => query(async move { Ok(json!(s.model_selection().await?)) }),
        "model.select" => Dispatch::Run(
            session.select_model(parameter(request.params.clone())?)?,
            false,
        ),
        "model.catalog" => {
            Dispatch::Run(session.catalog(parameter(request.params.clone())?)?, false)
        }
        "router.list" => query(async move { Ok(json!(s.managed_models().await?)) }),
        "router.request" => Dispatch::Run(
            session.manage_models(parameter(request.params.clone())?)?,
            false,
        ),
        "auth.request" => Dispatch::Run(
            session.authenticate(parameter(request.params.clone())?)?,
            true,
        ),
        "auth.status" => {
            let id: String = field(request, "operation_id")?;
            query(async move { Ok(json!(s.auth_status(&id).await?)) })
        }
        "auth.input" => {
            let id: String = field(request, "operation_id")?;
            let input = field(request, "input")?;
            query(async move { Ok(json!(s.submit_auth_input(&id, input).await?)) })
        }
        "interaction.respond" => {
            session.respond_interaction(
                field(request, "interaction_id")?,
                request.params.get("value").cloned().unwrap_or(Value::Null),
            )?;
            Dispatch::Immediate(json!({ "delivered": true }))
        }
        "session.copy" => {
            let options = eden_agent::CopyOptions {
                source: field(request, "source")?,
                destination: field(request, "destination")?,
                kind: field(request, "kind")?,
                target: field(request, "target")?,
                cwd: field(request, "cwd")?,
                public_only: request
                    .params
                    .get("public_only")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
            let apply = request
                .params
                .get("apply")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let composition = composition.to_path_buf();
            query(async move {
                let plan = Session::plan_copy(composition, options).await?;
                if apply {
                    Ok(json!({ "path": Session::apply_copy(plan).await? }))
                } else {
                    Ok(json!(plan))
                }
            })
        }
        _ => return Err(fault("UnknownMethod", "method is not supported")),
    };
    Ok(result)
}

async fn write_output<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut input: mpsc::Receiver<Value>,
    timeout: Duration,
) -> Result<(), Fault> {
    while let Some(value) = input.recv().await {
        let mut bytes =
            serde_json::to_vec(&value).map_err(|e| fault("OutputFailure", e.to_string()))?;
        bytes.push(b'\n');
        tokio::time::timeout(timeout, async {
            writer.write_all(&bytes).await?;
            writer.flush().await
        })
        .await
        .map_err(|_| fault("OutputTimeout", "client stopped reading output"))?
        .map_err(|e| fault("OutputFailure", e.to_string()))?;
    }
    tokio::time::timeout(timeout, writer.shutdown())
        .await
        .map_err(|_| fault("OutputTimeout", "output shutdown timed out"))?
        .map_err(|e| fault("OutputFailure", e.to_string()))
}

struct OpenOptions {
    composition: PathBuf,
    workspace: eden_agent::WorkspaceOptions,
}

fn replacement_options(session: &Session, request: &Request) -> Result<SessionOptions, Fault> {
    let history: Option<PathBuf> = field(request, "history")?;
    let open = request.method == "session.open";
    if open && history.as_ref().is_none_or(|path| !path.is_file()) {
        return Err(fault(
            "InvalidParams",
            "session.open requires an existing history file",
        ));
    }
    if !open && history.as_ref().is_some_and(|path| path.exists()) {
        return Err(fault(
            "InvalidParams",
            "session.new refuses existing history",
        ));
    }
    let cwd: Option<PathBuf> = field(request, "cwd")?;
    let cwd = if let Some(cwd) = cwd {
        cwd
    } else if open {
        let source = history
            .as_ref()
            .ok_or_else(|| fault("InvalidParams", "missing history"))?;
        let records = eden_kernel::history::read(source)?;
        PathBuf::from(
            records
                .first()
                .and_then(|record| record.payload["cwd"].as_str())
                .ok_or_else(|| fault("InvalidParams", "missing saved cwd"))?,
        )
    } else {
        PathBuf::from(session.cwd())
    };
    if !cwd.is_dir() {
        return Err(fault("InvalidParams", "cwd must be an existing directory"));
    }
    Ok(SessionOptions { cwd, history })
}

async fn serve<R, W>(
    mut session: Session,
    options: OpenOptions,
    input: R,
    writer: W,
    limits: Limits,
) -> Result<i32, Fault>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (output, receiver) = mpsc::channel(limits.output);
    let mut writer = tokio::spawn(write_output(writer, receiver, limits.write_timeout));
    let mut frames = Frames::new(input, limits.frame);
    let mut tasks: JoinSet<(String, u64, &'static str, Result<Value, Fault>)> = JoinSet::new();
    let mut pending = HashSet::new();
    let mut runs: HashMap<u64, (String, bool)> = HashMap::new();
    let mut sequence = 0;
    let mut shutdown_id = None;
    let mut exit_code = 0;
    let mut writer_completed = false;
    let signal = stop_signal();
    tokio::pin!(signal);
    let result = async {
        send(
            &output,
            json!({
                "version": 1,
                "type": "ready",
                "session_id": session.id(),
                "methods": METHODS,
                "limits": { "frame_bytes": limits.frame, "output_messages": limits.output },
                "interactions": false,
            }),
        )?;
        loop {
            tokio::select! {
                result = &mut writer => {
                    writer_completed = true;
                    return result.map_err(|e| fault("OutputFailure", e.to_string()))?;
                },
                code = &mut signal => {
                    exit_code = code;
                    break;
                },
                completed = tasks.join_next(), if !tasks.is_empty() => {
                    let (id, session_id, kind, value) = completed
                        .ok_or_else(|| fault("Unavailable", "missing request task"))?
                        .map_err(|e| fault("Unavailable", e.to_string()))?;
                    pending.remove(&id);
                    let reply = match value {
                        Ok(value) => response(
                            kind,
                            Some(&id),
                            session_id,
                            if kind == "accepted" {
                                value
                            } else {
                                json!({ "result": value })
                            },
                        ),
                        Err(e) => error(Some(&id), session_id, e),
                    };
                    send(&output, reply)?;
                },
                frame = frames.next() => {
                    let frame = match frame {
                        Ok(Some(frame)) => frame,
                        Ok(None) => break,
                        Err(e) => {
                            send(&output, error(None, session.id(), e.clone()))?;
                            return Err(e);
                        }
                    };
                    let request = match parse(&frame) {
                        Ok(request) => request,
                        Err((id, e)) => {
                            send(&output, error(id.as_deref(), session.id(), e))?;
                            continue;
                        }
                    };
                    if request.session_id != Some(session.id()) {
                        send(
                            &output,
                            error(
                                Some(&request.id),
                                session.id(),
                                fault(
                                    "SessionMismatch",
                                    "session_id must match the current ready identity",
                                ),
                            ),
                        )?;
                        continue;
                    }
                    if pending.contains(&request.id) {
                        send(
                            &output,
                            error(
                                Some(&request.id),
                                session.id(),
                                fault("DuplicateId", "request id is still pending"),
                            ),
                        )?;
                        continue;
                    }
                    if request.method == "shutdown" {
                        shutdown_id = Some(request.id);
                        break;
                    }
                    if matches!(request.method.as_str(), "session.new" | "session.open") {
                        if !tasks.is_empty() {
                            send(
                                &output,
                                error(
                                    Some(&request.id),
                                    session.id(),
                                    fault(
                                        "Busy",
                                        "wait for pending queries before replacing the session",
                                    ),
                                ),
                            )?;
                            continue;
                        }
                        let replacement = match replacement_options(&session, &request) {
                            Ok(options) => options,
                            Err(e) => {
                                send(&output, error(Some(&request.id), session.id(), e))?;
                                continue;
                            }
                        };
                        session.shutdown().await?;
                        for event in session.read_events(sequence).await? {
                            send(
                                &output,
                                json!({
                                    "version": 1,
                                    "type": "event",
                                    "session_id": session.id(),
                                    "event": event,
                                }),
                            )?;
                        }
                        let opened = Session::open_with_workspace(
                            &options.composition,
                            replacement,
                            options.workspace.clone(),
                        )
                        .await;
                        session = match opened {
                            Ok(session) => session,
                            Err(e) => {
                                send(&output, error(Some(&request.id), session.id(), e.clone()))?;
                                return Err(e);
                            }
                        };
                        sequence = 0;
                        pending.clear();
                        runs.clear();
                        send(
                            &output,
                            response(
                                "result",
                                Some(&request.id),
                                session.id(),
                                json!({ "result": { "session_id": session.id() } }),
                            ),
                        )?;
                        send(
                            &output,
                            json!({
                                "version": 1,
                                "type": "ready",
                                "session_id": session.id(),
                                "methods": METHODS,
                                "interactions": false,
                            }),
                        )?;
                        continue;
                    }
                    if pending.len() >= limits.pending
                        && !matches!(
                            request.method.as_str(),
                            "cancel"
                                | "shell.cancel"
                                | "interaction.respond"
                                | "state"
                                | "ping"
                                | "capabilities"
                        )
                    {
                        send(
                            &output,
                            error(
                                Some(&request.id),
                                session.id(),
                                fault("Busy", "too many pending requests"),
                            ),
                        )?;
                        continue;
                    }
                    match dispatch(&session, &request, &options.composition) {
                        Err(e) => send(&output, error(Some(&request.id), session.id(), e))?,
                        Ok(Dispatch::Immediate(value)) => send(
                            &output,
                            response(
                                "result",
                                Some(&request.id),
                                session.id(),
                                json!({ "result": value }),
                            ),
                        )?,
                        Ok(Dispatch::Run(run, private)) => {
                            send(
                                &output,
                                response(
                                    "accepted",
                                    Some(&request.id),
                                    session.id(),
                                    json!({ "run_id": run }),
                                ),
                            )?;
                            pending.insert(request.id.clone());
                            runs.insert(run, (request.id.clone(), private));
                            if private {
                                let s = session.clone();
                                tasks.spawn(async move {
                                    (
                                        request.id,
                                        s.id(),
                                        "private_result",
                                        s.wait(run).await.map(|terminal| json!(terminal)),
                                    )
                                });
                            }
                        }
                        Ok(Dispatch::Management(operation)) => {
                            pending.insert(request.id.clone());
                            let id = session.id();
                            tasks.spawn(
                                async move { (request.id, id, "accepted", operation.await) },
                            );
                        }
                        Ok(Dispatch::Query(query)) => {
                            pending.insert(request.id.clone());
                            let id = session.id();
                            tasks.spawn(async move { (request.id, id, "result", query.await) });
                        }
                    }
                },
                events = session.read_events(sequence) => {
                    let events = match events {
                        Ok(events) => events,
                        Err(e) => {
                            send(&output, error(None, session.id(), e.clone()))?;
                            return Err(e);
                        }
                    };
                    if events.is_empty() {
                        break;
                    }
                    for event in events {
                        sequence = event.sequence;
                        let id = runs.get(&event.run_id).map(|(id, _)| id.clone());
                        if event.kind == "settled"
                            && let Some((id, private)) = runs.remove(&event.run_id)
                            && !private
                        {
                            pending.remove(&id);
                        }
                        send(
                            &output,
                            json!({
                                "version": 1,
                                "type": "event",
                                "id": id,
                                "session_id": session.id(),
                                "event": event,
                            }),
                        )?;
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    let shutdown = session.shutdown().await;
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    let final_output = async {
        if result.is_ok() && shutdown.is_ok() {
            for event in session.read_events(sequence).await? {
                send(
                    &output,
                    json!({
                        "version": 1,
                        "type": "event",
                        "session_id": session.id(),
                        "event": event,
                    }),
                )?;
            }
            if let Some(id) = shutdown_id {
                send(
                    &output,
                    response(
                        "result",
                        Some(&id),
                        session.id(),
                        json!({ "result": { "closed": true } }),
                    ),
                )?;
            }
        }
        Ok::<(), Fault>(())
    }
    .await;
    drop(output);
    let written = if writer_completed {
        Ok(())
    } else {
        writer
            .await
            .map_err(|e| fault("OutputFailure", e.to_string()))?
    };
    result
        .and(shutdown)
        .and(final_output)
        .and(written)
        .map(|()| exit_code)
}

fn stop_signal() -> Pin<Box<dyn Future<Output = i32> + Send>> {
    #[cfg(unix)]
    {
        if let (Ok(mut interrupt), Ok(mut terminate), Ok(mut hangup)) = (
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()),
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()),
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()),
        ) {
            return Box::pin(async move {
                tokio::select! {
                    _ = interrupt.recv() => 130,
                    _ = terminate.recv() => 143,
                    _ = hangup.recv() => 129,
                }
            });
        }
    }
    Box::pin(async {
        let _ = tokio::signal::ctrl_c().await;
        130
    })
}

type WriteMessage = (Vec<u8>, tokio::sync::oneshot::Sender<std::io::Result<()>>);

/// A write completes only after the OS write and flush finish, so the timeout
/// measures the client pipe rather than an intermediate in-memory buffer.
struct StdioWriter {
    sender: Option<mpsc::Sender<WriteMessage>>,
    pending: Option<(usize, tokio::sync::oneshot::Receiver<std::io::Result<()>>)>,
}
impl StdioWriter {
    fn new() -> Self {
        let (sender, mut receiver) = mpsc::channel::<WriteMessage>(1);
        std::thread::spawn(move || {
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            while let Some((bytes, reply)) = receiver.blocking_recv() {
                let result = stdout.write_all(&bytes).and_then(|()| stdout.flush());
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
            }
        });
        Self {
            sender: Some(sender),
            pending: None,
        }
    }
}
impl AsyncWrite for StdioWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if this.pending.is_none() {
            let (reply, receiver) = tokio::sync::oneshot::channel();
            let Some(sender) = &this.sender else {
                return std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            };
            if sender.try_send((bytes.to_vec(), reply)).is_err() {
                return std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            }
            this.pending = Some((bytes.len(), receiver));
        }
        let Some((length, receiver)) = this.pending.as_mut() else {
            return std::task::Poll::Ready(Ok(0));
        };
        match Pin::new(receiver).poll(cx) {
            std::task::Poll::Pending => std::task::Poll::Pending,
            std::task::Poll::Ready(result) => {
                let length = *length;
                this.pending = None;
                std::task::Poll::Ready(
                    result
                        .unwrap_or_else(|_| Err(std::io::ErrorKind::BrokenPipe.into()))
                        .map(|()| length),
                )
            }
        }
    }
    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.sender.take();
        std::task::Poll::Ready(Ok(()))
    }
}

/// Own stdin/stdout for one RPC connection and await Session cleanup before returning.
/// Dedicated OS threads keep blocked terminal IO outside Tokio's shutdown barrier.
pub async fn run(cli: &Cli) -> Result<i32, Box<dyn std::error::Error>> {
    let options = OpenOptions {
        composition: crate::composition(cli)?,
        workspace: crate::workspace_options(cli),
    };
    let session = Session::open_with_workspace(
        &options.composition,
        crate::session_options(cli, cli.session.clone(), true)?,
        options.workspace.clone(),
    )
    .await?;
    let (input, mut input_bridge) = tokio::io::duplex(8192);
    let runtime = tokio::runtime::Handle::current();
    let input_runtime = runtime;
    let (read_finished, mut read_status) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut bytes = [0; 8192];
        let stdin = std::io::stdin();
        let mut stdin = stdin.lock();
        let result = loop {
            match stdin.read(&mut bytes) {
                Ok(0) => break Ok(()),
                Err(error) => break Err(error),
                Ok(n) => {
                    if input_runtime
                        .block_on(input_bridge.write_all(&bytes[..n]))
                        .is_err()
                    {
                        break Ok(());
                    }
                }
            }
        };
        let _ = read_finished.send(result);
    });
    let result = serve(
        session,
        options,
        input,
        StdioWriter::new(),
        Limits::default(),
    )
    .await?;
    if let Ok(read_result) = read_status.try_recv() {
        read_result?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn management_response<R: AsyncRead + Unpin>(frames: &mut Frames<R>, id: &str) -> Value {
        loop {
            let bytes = tokio::time::timeout(Duration::from_secs(5), frames.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            if value["id"] == id {
                return value;
            }
        }
    }

    #[tokio::test]
    async fn configuration_rpc_acknowledges_before_completion_without_chat_run() {
        use eden_agent::{WorkspaceOptions, embedded::Embedded};
        use eden_plugin_sdk::Package;
        use eden_protocol::{
            AGENT_LOOP, CONTEXT, Composition, PROVIDER, TOOL, configuration as cfg,
        };
        use std::sync::Arc;
        let release = Arc::new(tokio::sync::Notify::new());
        let update_release = release.clone();
        let package = Package::new("rpc-config")
            .service(AGENT_LOOP, |_: Value, _| async {
                Ok::<_, Fault>(Value::Null)
            })
            .service(CONTEXT, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
            .service(PROVIDER, |_: Value, _| async {
                Ok::<_, Fault>(Value::Null)
            })
            .service(TOOL, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
            .service(cfg::CONFIGURATION, move |request: cfg::PluginRequest, _| {
                let release = update_release.clone();
                async move {
                    match request {
                        cfg::PluginRequest::Describe => Ok(json!(cfg::Description {
                            schema: Some(json!({
                                "properties": { "limit": { "type": "integer", "minimum": 1 } },
                            })),
                            live_paths: vec!["/limit".into()],
                            secret_paths: vec!["/token".into()],
                            ..Default::default()
                        })),
                        cfg::PluginRequest::Validate { .. } => {
                            Ok(json!(cfg::Validation::default()))
                        }
                        cfg::PluginRequest::Update { .. } => {
                            release.notified().await;
                            Ok(json!(cfg::Validation::default()))
                        }
                    }
                }
            });
        let cwd = std::env::temp_dir().join(format!(
            "eden-rpc-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&cwd).unwrap();
        let composition: Composition = serde_json::from_value(json!({
            "packages": [],
            "roles": {},
            "runtime": {
                "instances": [{
                    "id": "rpc-config",
                    "package": "rpc-config",
                    "config": { "limit": 1, "token": "rpc-private-canary" },
                }],
            },
        }))
        .unwrap();
        let workspace = WorkspaceOptions {
            global_dir: cwd.join("global"),
            project_trust: Some(false),
            ..Default::default()
        };
        let session = Embedded::new(composition, cwd.clone())
            .package(package, "rpc-config-v1")
            .unwrap()
            .open(
                SessionOptions {
                    cwd: cwd.clone(),
                    history: None,
                },
                workspace.clone(),
            )
            .await
            .unwrap();
        let (client, server) = tokio::io::duplex(65536);
        let (server_input, server_output) = tokio::io::split(server);
        let (client_input, mut client_output) = tokio::io::split(client);
        let mut frames = Frames::new(client_input, 65536);
        let running = tokio::spawn(serve(
            session.clone(),
            OpenOptions {
                composition: cwd.join("unused.json"),
                workspace,
            },
            server_input,
            server_output,
            Limits::default(),
        ));
        for (id, method, params) in [
            ("inspect", "config.inspect", json!({})),
            (
                "validate",
                "config.validate",
                json!({ "instance": "rpc-config", "revision": 0, "patch": { "limit": 0 } }),
            ),
            (
                "preview",
                "config.preview",
                json!({ "instance": "rpc-config", "revision": 0, "patch": { "limit": 2 } }),
            ),
            (
                "apply",
                "config.apply",
                json!({ "instance": "rpc-config", "revision": 0, "patch": { "limit": 2 } }),
            ),
        ] {
            let request = json!({
                "version": 1,
                "id": id,
                "session_id": session.id(),
                "method": method,
                "params": params,
            });
            client_output
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            let reply = management_response(&mut frames, id).await;
            match id {
                "inspect" => {
                    assert_eq!(reply["result"]["revision"], 0);
                    assert!(!reply.to_string().contains("rpc-private-canary"));
                }
                "validate" => assert_eq!(reply["result"]["errors"][0]["path"], "/limit"),
                "preview" => assert_eq!(reply["result"]["application"], "live"),
                "apply" => {
                    assert_eq!(reply["type"], "accepted");
                    assert_eq!(reply["operation"], 1);
                    assert!(reply.get("run_id").is_none());
                }
                _ => unreachable!(),
            }
        }
        assert!(session.state().active_run.is_none());
        release.notify_one();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), session.wait_configuration(1))
                .await
                .unwrap()
                .unwrap()
                .status,
            eden_agent::configuration::Status::Applied
        );
        for (id, method, params) in [
            ("status", "config.status", json!({ "operation": 1 })),
            (
                "stale",
                "config.apply",
                json!({ "instance": "rpc-config", "revision": 0, "patch": { "limit": 3 } }),
            ),
            ("close", "shutdown", json!({})),
        ] {
            let request = json!({
                "version": 1,
                "id": id,
                "session_id": session.id(),
                "method": method,
                "params": params,
            });
            client_output
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            let reply = management_response(&mut frames, id).await;
            if id == "status" {
                assert_eq!(reply["result"]["status"], "applied");
            }
            if id == "stale" {
                assert_eq!(reply["error"]["code"], "Conflict");
            }
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), running)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            0
        );
        std::fs::remove_dir_all(cwd).unwrap();
    }

    #[tokio::test]
    async fn framing_preserves_unicode_crlf_and_eof_tail() {
        let bytes = "{\"x\":\"a\u{2028}b\u{2029}中文\"}\r\nlast";
        let mut reader = Frames::new(bytes.as_bytes(), 100);
        assert_eq!(
            String::from_utf8(reader.next().await.unwrap().unwrap()).unwrap(),
            "{\"x\":\"a\u{2028}b\u{2029}中文\"}"
        );
        assert_eq!(reader.next().await.unwrap().unwrap(), b"last");
        assert!(reader.next().await.unwrap().is_none());
    }
    #[tokio::test]
    async fn oversized_frame_is_bounded_before_eof() {
        let mut reader = Frames::new(&b"0123456789"[..], 4);
        assert_eq!(reader.next().await.unwrap_err().code, "FrameTooLarge");
        assert!(reader.buffer.len() <= 4);
    }
    #[tokio::test]
    async fn blocked_writer_times_out() {
        let (writer, _unread) = tokio::io::duplex(1);
        let (tx, rx) = mpsc::channel(1);
        tx.send(json!({ "large": "message" })).await.unwrap();
        assert_eq!(
            write_output(writer, rx, Duration::from_millis(20))
                .await
                .unwrap_err()
                .code,
            "OutputTimeout"
        );
    }
    #[test]
    fn malformed_and_wrong_version_frames_have_safe_errors() {
        assert_eq!(parse(b"\xff").err().unwrap().1.code, "InvalidRequest");
        let (id, error) = parse(br#"{"version":2,"id":"v","method":"ping"}"#)
            .err()
            .unwrap();
        assert_eq!(id.as_deref(), Some("v"));
        assert_eq!(error.code, "UnsupportedVersion");
    }
}
