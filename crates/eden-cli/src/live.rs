//! Explicit loopback host for one live Session; connection lifetime does not own execution.
use crate::cli::Cli;
use eden_agent::{Fault, Session};
use eden_protocol::presentation::ActionRequest;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
};

#[path = "live_management.rs"]
mod management;
#[path = "live_previews.rs"]
mod previews;
use management::{Management, detach_scans, management_read, management_submit};

const MAX_FRAME: usize = 1024 * 1024;
#[derive(serde::Serialize, serde::Deserialize)]
struct Endpoint {
    address: String,
    token: String,
    session_id: u64,
}
enum Submission {
    Running {
        body: Value,
        listeners: Vec<tokio::sync::oneshot::Sender<Result<Value, Fault>>>,
    },
    Done {
        body: Value,
        result: Result<Value, Fault>,
    },
}
struct Shared {
    management: Management,
    session: Session,
    token: String,
    web_root: Option<PathBuf>,
    submissions: Mutex<(HashMap<String, Submission>, VecDeque<String>)>,
    stop: watch::Sender<bool>,
    history: tokio::sync::Mutex<Option<(u64, Arc<Vec<eden_protocol::coding::Record>>)>>,
}
fn fault(code: &str, message: impl Into<String>) -> Fault {
    Fault::new(code, "live", message)
}
fn field<T: serde::de::DeserializeOwned>(body: &Value, key: &str) -> Result<T, Fault> {
    serde_json::from_value(body.get(key).cloned().unwrap_or(Value::Null))
        .map_err(|error| fault("InvalidInput", error.to_string()))
}
fn token() -> Result<String, Fault> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| fault("Unavailable", error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
fn write_endpoint(path: &Path, endpoint: &Endpoint) -> Result<(), Fault> {
    if path.exists() {
        return Err(fault("InvalidInput", "endpoint path already exists"));
    }
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| fault("FileFailure", error.to_string()))?;
    let bytes =
        serde_json::to_vec(endpoint).map_err(|error| fault("InvalidInput", error.to_string()))?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| fault("FileFailure", error.to_string()))?;
        file.write_all(&bytes)
            .map_err(|error| fault("FileFailure", error.to_string()))?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, bytes).map_err(|error| fault("FileFailure", error.to_string()))?;
    Ok(())
}
/// Run an explicit single-user host. Only `/shutdown` or a process signal closes its Session.
pub async fn run_host(
    cli: &Cli,
    endpoint_path: &Path,
    web_root: Option<&Path>,
) -> Result<i32, Box<dyn std::error::Error>> {
    if cli.session.as_ref().is_some_and(|history| history.exists()) {
        return Err(
            "eden live starts a new task; an existing history is not a live attachment. Use an \
             explicit history or session command instead."
                .into(),
        );
    }
    let (composition, selected) = crate::load_composition(cli)?;
    let mut options = crate::session_options(cli, cli.session.clone(), true)?;
    options.cwd = std::fs::canonicalize(options.cwd)?;
    if !cli.no_session
        && options.history.is_none()
        && selected.roles.contains_key(eden_protocol::coding::LOOP)
    {
        options.history = Some(default_history(&options.cwd)?);
    }
    let session =
        Session::open_with_workspace(composition, options, crate::prompt_options(cli)).await?;
    host_session(cli, session, endpoint_path, web_root).await
}
/// Reopen an explicitly selected stopped history without resuming its model loop or queued inputs.
/// Saved cwd and CLI resource/trust overrides follow the same authority as ordinary prompt startup.
pub async fn run_saved_host(
    cli: &Cli,
    endpoint_path: &Path,
    web_root: Option<&Path>,
) -> Result<i32, Box<dyn std::error::Error>> {
    let history = cli
        .session
        .clone()
        .ok_or("resuming a live host requires --session HISTORY")?;
    let composition = crate::composition(cli)?;
    let session = Session::open_saved_with_workspace(
        composition,
        history,
        cli.cwd.clone(),
        crate::prompt_options(cli),
    )
    .await?;
    host_session(cli, session, endpoint_path, web_root).await
}
fn default_history(cwd: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let root = cwd.join(".eden/sessions");
    std::fs::create_dir_all(&root)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    Ok(root.join(format!("{stamp}-{}.jsonl", std::process::id())))
}
async fn host_session(
    cli: &Cli,
    session: Session,
    endpoint_path: &Path,
    web_root: Option<&Path>,
) -> Result<i32, Box<dyn std::error::Error>> {
    let result = serve_session(cli, &session, endpoint_path, web_root).await;
    let closed = session.shutdown().await;
    match result {
        Ok(code) => {
            closed?;
            Ok(code)
        }
        Err(error) => Err(error),
    }
}
async fn serve_session(
    cli: &Cli,
    session: &Session,
    endpoint_path: &Path,
    web_root: Option<&Path>,
) -> Result<i32, Box<dyn std::error::Error>> {
    if let Some(identity) = &cli.model {
        let (provider, model) = identity
            .split_once('/')
            .ok_or("--model requires PROVIDER/MODEL")?;
        let run = session.select_model(eden_protocol::models::ModelSelection {
            provider: provider.into(),
            model: model.into(),
            thinking: cli.thinking.clone(),
        })?;
        session.wait(run).await?.into_result()?;
    }
    session.set_interactions(true);
    session.enable_shared_presentation();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = Endpoint {
        address: listener.local_addr()?.to_string(),
        token: token()?,
        session_id: session.id(),
    };
    if let Err(error) = write_endpoint(endpoint_path, &endpoint) {
        session.shutdown().await?;
        return Err(Box::new(error));
    }
    let _registration = crate::tui::Registration::new(session, endpoint_path)?;
    let _ = session.refresh_models_in_background().await;
    let (stop, mut stopped) = watch::channel(false);
    let shared = Arc::new(Shared {
        management: Management {
            launch: Some(cli.clone()),
            ..Default::default()
        },
        session: session.clone(),
        token: endpoint.token,
        web_root: web_root.map(Path::to_owned),
        submissions: Mutex::new((HashMap::new(), VecDeque::new())),
        history: tokio::sync::Mutex::new(None),
        stop,
    });
    println!(
        "{}",
        json!({
            "endpoint": endpoint_path,
            "session_id": session.id(),
            "address": endpoint.address,
        })
    );
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let shared = shared.clone();
                tokio::spawn(async move {
                    let _ = serve(stream, Host::Live(shared)).await;
                });
            },
            _ = stopped.changed() => {
                if *stopped.borrow() {
                    break;
                }
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    let closed = shared.session.shutdown().await;
    let removed = std::fs::remove_file(endpoint_path);
    closed?;
    removed?;
    Ok(0)
}
enum Host {
    Live(Arc<Shared>),
    Static(Arc<StaticShared>),
}
impl Host {
    fn token(&self) -> &str {
        match self {
            Self::Live(shared) => &shared.token,
            Self::Static(shared) => &shared.token,
        }
    }
    fn web_root(&self) -> Option<&Path> {
        match self {
            Self::Live(shared) => shared.web_root.as_deref(),
            Self::Static(shared) => shared.web_root.as_deref(),
        }
    }
    async fn dispatch(&self, method: &str, path: &str, body: Value) -> Result<Value, Fault> {
        match self {
            Self::Live(shared) => dispatch(shared, method, path, body).await,
            Self::Static(shared) => {
                check_session(&body, shared.snapshot.session_id)?;
                dispatch_static(shared, method, path).await
            }
        }
    }
}
async fn serve(mut stream: TcpStream, host: Host) -> Result<(), Fault> {
    let request = read_request(&mut stream).await;
    let (status, mime, bytes) = match request {
        Ok((method, path, headers, body)) => {
            let route = path.split('?').next().unwrap_or("/");
            if method == "GET" && (route == "/" || route.starts_with("/assets/")) {
                match static_file(host.web_root(), host.token(), route, &path) {
                    Ok((mime, bytes)) => (200, mime, bytes),
                    Err(error) => json_error(error),
                }
            } else if headers.get("x-eden-token").map(String::as_str) != Some(host.token()) {
                json_error(fault("Unauthorized", "invalid host token"))
            } else {
                match host.dispatch(&method, &path, body).await {
                    Ok(value) => (
                        200,
                        "application/json",
                        serde_json::to_vec(&json!({ "ok": true, "result": value })).unwrap(),
                    ),
                    Err(error) => json_error(error),
                }
            }
        }
        Err(error) => json_error(error),
    };
    let header = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: \
         close\r\nCache-Control: no-store\r\n\r\n",
        if status == 200 { "OK" } else { "Error" },
        bytes.len()
    );
    stream
        .write_all(header.as_bytes())
        .await
        .map_err(|error| fault("OutputFailure", error.to_string()))?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|error| fault("OutputFailure", error.to_string()))?;
    Ok(())
}
fn json_error(error: Fault) -> (u16, &'static str, Vec<u8>) {
    let status = if error.code == "Unauthorized" {
        401
    } else {
        400
    };
    (
        status,
        "application/json",
        serde_json::to_vec(&json!({ "ok": false, "error": error })).unwrap(),
    )
}
async fn read_request(
    stream: &mut TcpStream,
) -> Result<(String, String, HashMap<String, String>, Value), Fault> {
    let mut bytes = Vec::new();
    let end = loop {
        if bytes.len() > MAX_FRAME {
            return Err(fault("FrameTooLarge", "request too large"));
        }
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        let mut chunk = [0u8; 4096];
        let count = stream
            .read(&mut chunk)
            .await
            .map_err(|error| fault("InputFailure", error.to_string()))?;
        if count == 0 {
            return Err(fault("InvalidInput", "incomplete HTTP request"));
        }
        bytes.extend_from_slice(&chunk[..count]);
    };
    let header = std::str::from_utf8(&bytes[..end])
        .map_err(|error| fault("InvalidInput", error.to_string()))?
        .to_owned();
    let mut lines = header.split("\r\n");
    let request = lines
        .next()
        .ok_or_else(|| fault("InvalidInput", "missing request line"))?;
    let parts: Vec<_> = request.split(' ').collect();
    if parts.len() != 3 || parts[2] != "HTTP/1.1" {
        return Err(fault("InvalidInput", "expected HTTP/1.1 request"));
    }
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| fault("InvalidInput", "invalid header"))?;
        headers.insert(key.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = headers.get("content-length").map_or(Ok(0), |value| {
        value
            .parse::<usize>()
            .map_err(|_| fault("InvalidInput", "invalid content length"))
    })?;
    if length > MAX_FRAME {
        return Err(fault("FrameTooLarge", "request body too large"));
    }
    while bytes.len() < end + length {
        let mut chunk = [0u8; 4096];
        let count = stream
            .read(&mut chunk)
            .await
            .map_err(|error| fault("InputFailure", error.to_string()))?;
        if count == 0 {
            return Err(fault("InvalidInput", "incomplete request body"));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let body = if length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[end..end + length])
            .map_err(|error| fault("InvalidInput", error.to_string()))?
    };
    Ok((parts[0].to_owned(), parts[1].to_owned(), headers, body))
}
fn static_file(
    web_root: Option<&Path>,
    token: &str,
    route: &str,
    path: &str,
) -> Result<(&'static str, Vec<u8>), Fault> {
    if route == "/" && !path.contains(&format!("token={token}")) {
        return Err(fault(
            "Unauthorized",
            "open the explicit URL from the endpoint file",
        ));
    }
    let root = web_root.ok_or_else(|| fault("Unavailable", "built web adapter not configured"))?;
    let relative = if route == "/" {
        "index.html"
    } else {
        route.trim_start_matches('/')
    };
    if relative.split('/').any(|part| part == ".." || part == ".") {
        return Err(fault("InvalidInput", "invalid asset path"));
    }
    let path = root.join(relative);
    let bytes = std::fs::read(path).map_err(|error| fault("FileFailure", error.to_string()))?;
    let mime = if relative.ends_with(".js") {
        "text/javascript"
    } else if relative.ends_with(".css") {
        "text/css"
    } else {
        "text/html"
    };
    Ok((mime, bytes))
}
fn query_parameter<'a>(path: &'a str, name: &str) -> Option<&'a str> {
    path.split('?').nth(1)?.split('&').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key == name).then_some(value)
    })
}
async fn dispatch(
    shared: &Arc<Shared>,
    method: &str,
    path: &str,
    body: Value,
) -> Result<Value, Fault> {
    let session = &shared.session;
    check_session(&body, session.id())?;
    let route = path.split('?').next().unwrap_or(path);
    match (method, route) {
        ("POST", "/attach") => {
            let frontend: String = field(&body, "frontend")?;
            let attachment = session.attach_presentation(&frontend)?;
            let owner = shared.clone();
            tokio::spawn(async move {
                owner.session.wait_presentation_detach(attachment).await;
                previews::detach(&owner, attachment).await;
                detach_scans(&owner, attachment).await;
            });
            Ok(json!({ "attachment": attachment }))
        }
        ("POST", "/detach") => {
            let attachment = field(&body, "attachment")?;
            if let Err(error) = session.detach_presentation(attachment)
                && error.code != "InvalidInput"
            {
                return Err(error);
            }
            previews::detach(shared, attachment).await;
            detach_scans(shared, attachment).await;
            Ok(json!({ "detached": true }))
        }
        ("POST", "/activity") => {
            session.presentation_activity(
                field(&body, "attachment")?,
                field(&body, "target")?,
                field(&body, "active")?,
            )?;
            Ok(json!({ "updated": true }))
        }
        ("GET", "/snapshot") => {
            let after = query_parameter(path, "after").and_then(|value| value.parse::<u64>().ok());
            if let Some(attachment) =
                query_parameter(path, "attachment").and_then(|value| value.parse::<u64>().ok())
            {
                session.presentation_heartbeat(attachment)?;
            }
            if let Some(sequence) = after {
                let _ = tokio::time::timeout(
                    Duration::from_secs(4),
                    session.presentation_changed(sequence),
                )
                .await;
            }
            Ok(json!({ "presentation": session.presentation_snapshot(), "state": session.state() }))
        }
        ("GET", "/tui/snapshot") => {
            if let Some(attachment) =
                query_parameter(path, "attachment").and_then(|v| v.parse().ok())
            {
                session
                    .presentation_heartbeat(attachment)
                    .map_err(|error| {
                        if error.code == "InvalidInput" {
                            fault(
                                "AttachmentExpired",
                                "attachment lease ended; attach again to this session",
                            )
                        } else {
                            error
                        }
                    })?;
            }
            if let (Some(after), Some(events_after)) = (
                query_parameter(path, "after").and_then(|v| v.parse().ok()),
                query_parameter(path, "events_after").and_then(|v| v.parse().ok()),
            ) {
                tokio::select! {
                    _ = session.presentation_changed(after) => {},
                    _ = wait_events(session, events_after) => {},
                    _ = tokio::time::sleep(Duration::from_secs(4)) => {}
                }
            }
            // Read events before history: a commit that races this read can appear in history
            // first, but its live event remains available to the next cursor read.
            let events = session.events();
            let history = snapshot_history(shared, &events).await?;
            let incremental = query_parameter(path, "incremental") == Some("1");
            let after = query_parameter(path, "events_after")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let reset = !incremental
                || events
                    .first()
                    .is_some_and(|e| e.sequence > after.saturating_add(1));
            let head = history.last().map_or(0, |record| record.sequence);
            let unchanged = !reset
                && query_parameter(path, "history_head").and_then(|v| v.parse::<u64>().ok())
                    == Some(head);
            let first = events.first().map(|event| event.sequence);
            let events = events
                .into_iter()
                .filter(|event| reset || event.sequence > after)
                .collect::<Vec<_>>();
            let wire_history = if unchanged {
                serde_json::Value::Array(Vec::new())
            } else {
                serde_json::to_value(&history)
                    .map_err(|error| fault("InvalidInput", error.to_string()))?
            };
            Ok(json!({
                "presentation": session.presentation_snapshot(),
                "state": session.state(),
                "history": wire_history,
                "history_unchanged": unchanged,
                "events_reset": reset,
                "events_first": first,
                "events": events,
            }))
        }
        ("POST", "/request-status") => {
            let id: String = field(&body, "request_id")?;
            let guard = shared.submissions.lock().unwrap_or_else(|e| e.into_inner());
            Ok(match guard.0.get(&id) {
                None => json!({ "status": "unknown" }),
                Some(Submission::Running { .. }) => json!({ "status": "running" }),
                Some(Submission::Done { result, .. }) => json!({
                    "status": "done",
                    "result": result,
                }),
            })
        }
        (
            "POST",
            "/prompt"
            | "/enqueue"
            | "/shell"
            | "/command"
            | "/queue/withdraw"
            | "/queue/configure"
            | "/context/apply"
            | "/context/rebuild"
            | "/context/compact"
            | "/context/images"
            | "/models/select"
            | "/models/cycle"
            | "/models/catalog"
            | "/router/manage"
            | "/auth/start"
            | "/manage/navigate"
            | "/manage/metadata"
            | "/resources/reload"
            | "/background/warmer"
            | "/manage/open"
            | "/manage/delete"
            | "/manage/copy/preview"
            | "/manage/copy/apply"
            | "/manage/copy/discard"
            | "/delivery/preview"
            | "/delivery/publish"
            | "/delivery/discard"
            | "/updates"
            | "/delivery/save"
            | "/trust/save"
            | "/configuration/apply"
            | "/manage/rename",
        ) => submit_once(shared, route, body).await,
        (
            "POST",
            "/manage/sessions"
            | "/manage/sessions/start"
            | "/manage/sessions/poll"
            | "/manage/sessions/cancel"
            | "/manage/tree"
            | "/trust/inspect"
            | "/configuration/wait",
        ) => management_read(shared, route, &body).await,
        ("POST", "/models/list") => Ok(json!(session.models().await?)),
        ("POST", "/models/current") => {
            let effective = session.effective_model().await?;
            Ok(json!({
                "selection": session.model_selection().await?,
                "effective_target": effective.target,
                "selection_source": effective.selection_source,
                "last_committed_target": session
                    .history()
                    .await?
                    .iter()
                    .rev()
                    .find(|r| r.kind == "model_selection")
                    .map(|r| &r.payload["target"]),
            }))
        }
        ("POST", "/router/list") => Ok(json!(session.managed_models().await?)),
        ("POST", "/auth/methods") => Ok(json!(
            session
                .authentication_methods(field(&body, "provider")?)
                .await?
        )),
        ("POST", "/auth/status") => Ok(json!(
            session
                .auth_status(&field::<String>(&body, "operation_id")?)
                .await?
        )),
        ("POST", "/auth/input") => {
            let operation_id = field::<String>(&body, "operation_id")?;
            let input = field::<String>(&body, "input")?;
            if body["api_key"] == true {
                Ok(json!({
                    "run_id": session.authenticate(eden_protocol::models::AuthRequest::Input {
                            operation_id,
                            api_key: input
                        })?,
                }))
            } else {
                Ok(json!(
                    session.submit_auth_input(&operation_id, input).await?
                ))
            }
        }
        ("POST", "/session/catalog") => Ok(json!(session.session_catalog().await?)),
        ("POST", "/session/branches") => Ok(json!(
            session
                .session_branches(field::<String>(&body, "path")?)
                .await?
        )),
        ("POST", "/reference/check") => Ok(json!(
            session
                .check_reference_input(content(&body)?, field(&body, "references")?)
                .await?
        )),
        ("POST", "/reference/preview") => Ok(json!(
            session
                .reference_preview(field::<String>(&body, "path")?, body["head"].as_u64())
                .await?
        )),
        ("POST", "/context/inspect") => Ok(json!(session.inspect_context().await?)),
        ("POST", "/queue/inspect") => Ok(json!(session.queued().await?)),
        ("POST", "/resources") => Ok(json!(session.resources().await?)),
        ("POST", "/tools") => Ok(json!(session.tools().await?)),
        ("POST", "/commands") => Ok(json!(session.commands().await?)),
        ("POST", "/shell/cancel") => {
            session.cancel_shell(field(&body, "run_id")?)?;
            Ok(json!({ "cancel_requested": true }))
        }
        ("POST", "/configuration/inspect") => Ok(json!(session.inspect_configuration().await?)),
        ("POST", "/configuration/open") => {
            let instance: String = field(&body, "instance")?;
            Ok(json!(session.open_configuration(&instance).await?))
        }
        ("POST", "/private-input") => {
            let input = serde_json::from_value(body)
                .map_err(|_| fault("InvalidInput", "invalid private input request"))?;
            session.submit_private_input(input).await
        }
        ("POST", "/action") => {
            let action: ActionRequest = serde_json::from_value(body)
                .map_err(|error| fault("InvalidInput", error.to_string()))?;
            session.presentation_action(action).await
        }
        ("POST", "/interaction") => {
            session.respond_interaction(field(&body, "interaction_id")?, field(&body, "value")?)?;
            Ok(json!({ "delivered": true }))
        }
        ("POST", "/terminal") => {
            let run_id: u64 = field(&body, "run_id")?;
            Ok(json!(session.wait(run_id).await?))
        }
        ("POST", "/cancel") => {
            session.cancel(field(&body, "run_id")?)?;
            Ok(json!({ "cancel_requested": true }))
        }
        ("POST", "/shutdown") => {
            shared.stop.send_replace(true);
            Ok(json!({ "stopping": true }))
        }
        _ => Err(fault("Unsupported", "unknown live host operation")),
    }
}
#[path = "reading.rs"]
mod reading;
use reading::read_document;

struct StaticShared {
    reading: Option<eden_protocol::delivery::ReadingDocument>,
    diagnostic: Option<String>,
    history: Vec<eden_protocol::coding::Record>,
    snapshot: eden_protocol::presentation::Snapshot,
    token: String,
    web_root: Option<PathBuf>,
    stop: watch::Sender<bool>,
}
/// Serve one immutable committed history without loading its saved plugin composition.
pub async fn run_read_host(
    history: &Path,
    endpoint_path: &Path,
    web_root: Option<&Path>,
) -> Result<i32, Box<dyn std::error::Error>> {
    let document = read_document(history)?;
    let records = &document.history;
    let session_id = records
        .first()
        .map(|record| record.session_id)
        .unwrap_or(u64::from_str_radix(&token()?[..16], 16)?);
    let snapshot = eden_protocol::presentation::Snapshot {
        version: eden_protocol::presentation::VERSION,
        session_id,
        sequence: 0,
        views: eden_protocol::presentation::static_views(
            records,
            &eden_protocol::delivery::Selection {
                attachments: true,
                full_outputs: true,
                thinking: true,
                ..Default::default()
            },
        ),
        activity: vec![],
        pending_interactions: vec![],
    };
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = Endpoint {
        address: listener.local_addr()?.to_string(),
        token: token()?,
        session_id,
    };
    write_endpoint(endpoint_path, &endpoint)?;
    let (stop, mut stopped) = watch::channel(false);
    let shared = Arc::new(StaticShared {
        reading: document.reading,
        diagnostic: document.diagnostic,
        history: document.history,
        snapshot,
        token: endpoint.token,
        web_root: web_root.map(Path::to_owned),
        stop,
    });
    println!(
        "{}",
        json!({
            "endpoint": endpoint_path,
            "session_id": session_id,
            "address": endpoint.address,
            "read_only": true,
        })
    );
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let shared = shared.clone();
                tokio::spawn(async move {
                    let _ = serve(stream, Host::Static(shared)).await;
                });
            },
            _ = stopped.changed() => if *stopped.borrow() {
                    break;
                },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    std::fs::remove_file(endpoint_path)?;
    Ok(0)
}
async fn dispatch_static(shared: &StaticShared, method: &str, path: &str) -> Result<Value, Fault> {
    let route = path.split('?').next().unwrap_or(path);
    match (method, route) {
        ("POST", "/attach") => Ok(json!({ "attachment": 1 })),
        ("POST", "/detach") => Ok(json!({ "detached": true })),
        ("GET", "/snapshot" | "/tui/snapshot") => {
            if query_parameter(path, "after").is_some() {
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
            Ok(json!({
                "presentation": shared.snapshot,
                "state": {
                    "session_id": shared.snapshot.session_id,
                    "active_run": null,
                    "closed": true,
                    "read_only": true,
                },
                "history": shared.history,
                "reading": shared.reading,
                "diagnostic": shared.diagnostic,
                "events": [],
            }))
        }
        ("POST", "/shutdown") => {
            shared.stop.send_replace(true);
            Ok(json!({ "stopping": true }))
        }
        _ => Err(fault("Unsupported", "saved presentation is read-only")),
    }
}
/// A small HTTP client for the terminal adapter and installed process probe.
pub async fn call(
    endpoint_path: &Path,
    method: &str,
    route: &str,
    body: Option<&Value>,
) -> Result<Value, Fault> {
    eden_tui_client::call(endpoint_path, method, route, body).await
}
/// Start the terminal adapter against one explicitly selected host.
pub async fn run_tui(endpoint_path: &Path) -> Result<i32, Box<dyn std::error::Error>> {
    crate::tui::attach(endpoint_path, None, "terminal", false).await
}

// Public submission bodies only. Private input continues through the dedicated Session path.
async fn submit_once(shared: &Arc<Shared>, route: &str, body: Value) -> Result<Value, Fault> {
    let id: String = field(&body, "request_id")?;
    if id.is_empty() || id.len() > 256 {
        return Err(fault("InvalidInput", "invalid request id"));
    }
    if route == "/auth/start"
        && matches!(body["request"]["action"].as_str(), Some("input" | "submit"))
    {
        return Err(fault(
            "InvalidInput",
            "private authentication input requires /auth/input",
        ));
    }
    let fingerprint = json!({ "route": route, "body": body });
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let start = {
        let mut guard = shared.submissions.lock().unwrap_or_else(|e| e.into_inner());
        match guard.0.get_mut(&id) {
            Some(Submission::Done {
                body: prior,
                result,
            }) => {
                return if *prior == fingerprint {
                    result.clone()
                } else {
                    Err(fault(
                        "DuplicateId",
                        "request id reused with different submission",
                    ))
                };
            }
            Some(Submission::Running {
                body: prior,
                listeners,
            }) => {
                if *prior != fingerprint {
                    return Err(fault(
                        "DuplicateId",
                        "request id reused with different submission",
                    ));
                }
                listeners.push(sender);
                false
            }
            None => {
                guard.0.insert(
                    id.clone(),
                    Submission::Running {
                        body: fingerprint.clone(),
                        listeners: vec![sender],
                    },
                );
                true
            }
        }
    };
    if start {
        let shared = shared.clone();
        let route = route.to_owned();
        tokio::spawn(async move {
            let result = perform_submission(&shared, &route, &body).await;
            let mut guard = shared.submissions.lock().unwrap_or_else(|e| e.into_inner());
            let result = previews::completed_result(&shared, &route, &body, result);
            let listeners = match guard.0.remove(&id) {
                Some(Submission::Running { listeners, .. }) => listeners,
                _ => vec![],
            };
            for listener in listeners {
                let _ = listener.send(result.clone());
            }
            guard.0.insert(
                id.clone(),
                Submission::Done {
                    body: fingerprint,
                    result,
                },
            );
            guard.1.push_back(id);
            while guard.1.len() > 1024 {
                if let Some(old) = guard.1.pop_front() {
                    guard.0.remove(&old);
                }
            }
        });
    }
    receiver
        .await
        .map_err(|_| fault("Unavailable", "submission result lost"))?
}
fn content(body: &Value) -> Result<Vec<eden_protocol::coding::Block>, Fault> {
    if body.get("content").is_some() {
        field(body, "content")
    } else {
        Ok(vec![eden_protocol::coding::Block::Text {
            text: field(body, "text")?,
        }])
    }
}
async fn perform_submission(shared: &Shared, route: &str, body: &Value) -> Result<Value, Fault> {
    let session = &shared.session;
    match route {
        "/manage/open"
        | "/manage/delete"
        | "/manage/copy/preview"
        | "/manage/copy/apply"
        | "/manage/copy/discard"
        | "/delivery/preview"
        | "/delivery/publish"
        | "/delivery/discard"
        | "/updates"
        | "/delivery/save"
        | "/trust/save"
        | "/configuration/apply"
        | "/manage/rename" => management_submit(shared, route, body).await,
        "/background/warmer" => Ok(json!(session.cache_warmer(field(body, "cancel")?).await?)),
        "/models/cycle" => {
            let models: Vec<_> = session
                .models()
                .await?
                .models
                .into_iter()
                .filter(|m| m.status == "configured")
                .collect();
            if models.is_empty() {
                return Err(fault("ModelUnavailable", "no configured models available"));
            }
            let current = session.model_selection().await?;
            let index = current
                .and_then(|c| {
                    models
                        .iter()
                        .position(|m| m.target.provider == c.provider && m.target.model == c.model)
                })
                .map_or(0, |i| (i + 1) % models.len());
            let target = &models[index].target;
            Ok(json!({
                "run_id": session.select_model(eden_protocol::models::ModelSelection {
                        provider: target.provider.clone(),
                        model: target.model.clone(),
                        thinking: target.thinking.requested.clone()
                    })?,
            }))
        }
        "/models/select" => Ok(json!({
            "run_id": session.select_model_with_default(
                field(body, "selection")?,
                body["save_default"] == true
            )?,
        })),
        "/models/catalog" => Ok(json!({ "run_id": session.catalog(field(body, "request")?)? })),
        "/router/manage" => {
            Ok(json!({ "run_id": session.manage_models(field(body, "request")?)? }))
        }
        "/auth/start" => {
            let request = field::<eden_protocol::models::AuthRequest>(body, "request")?;
            if matches!(
                request,
                eden_protocol::models::AuthRequest::Input { .. }
                    | eden_protocol::models::AuthRequest::Submit { .. }
            ) {
                return Err(fault(
                    "InvalidInput",
                    "private authentication input requires /auth/input",
                ));
            }
            Ok(json!({ "run_id": session.authenticate(request)? }))
        }
        "/manage/navigate" => Ok(json!({
            "run_id": session.navigate(
                field(body, "target")?,
                field(body, "branch")?,
                field(body, "summarize")?
            )?,
        })),
        "/manage/metadata" => Ok(json!({
            "run_id":
                session.set_metadata(field(body, "name")?, field(body, "tags")?)?,
        })),
        "/resources/reload" => Ok(json!({ "run_id": session.reload_resources()? })),
        "/context/images" => Ok(json!(session.edit_images(field(body, "edit")?).await?)),
        "/context/rebuild" => Ok(json!({
            "run_id": session.rebuild_context(field(body, "rebuild")?)?,
        })),
        "/context/compact" => Ok(json!({
            "run_id":
                session.compact(body["instructions"].as_str().unwrap_or_default().into())?,
        })),
        "/context/apply" => Ok(json!(session.edit_context(field(body, "edit")?).await?)),
        "/prompt" => {
            let content = content(body)?;
            let references: Vec<eden_protocol::session_reference::Reference> = body
                .get("references")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|error| fault("InvalidReference", error.to_string()))?
                .unwrap_or_default();
            if session.has_role(eden_protocol::context_edit::SERVICE)
                && session.state().active_run.is_none()
            {
                session
                    .check_reference_input(content.clone(), references.clone())
                    .await?;
            }
            Ok(json!({ "run_id": session.submit_referenced(content, references)? }))
        }
        "/enqueue" => {
            let kind: String = field(body, "kind")?;
            if !["steering", "follow_up"].contains(&kind.as_str()) {
                return Err(fault("InvalidInput", "choose steering or follow_up"));
            }
            Ok(json!(
                session
                    .enqueue_referenced(
                        &kind,
                        content(body)?,
                        body.get("references")
                            .cloned()
                            .map(serde_json::from_value)
                            .transpose()
                            .map_err(|error| fault("InvalidReference", error.to_string()))?
                            .unwrap_or_default()
                    )
                    .await?
            ))
        }
        "/shell" => Ok(json!({
            "run_id": session.user_shell(
                field(body, "command")?,
                field(body, "shell")?,
                field(body, "exclude_from_context")?
            )?,
        })),
        "/command" => Ok(json!({
            "run_id": session.command(field(body, "name")?, field(body, "arguments")?)?,
        })),
        "/queue/configure" => Ok(json!({
            "run_id":
                session.configure_queue(field(body, "steering")?, field(body, "follow_up")?)?,
        })),
        "/queue/withdraw" => Ok(json!(session.withdraw_queue(field(body, "ids")?).await?)),
        _ => Err(fault("Unsupported", "unknown submission")),
    }
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;

// A closed/fully expired event ledger must not turn an idle long poll into a busy loop.
async fn wait_events(session: &Session, after: u64) {
    match session.read_events(after).await {
        Ok(events) if !events.is_empty() => {}
        Err(_)
            if session
                .events()
                .last()
                .is_some_and(|event| event.sequence > after) => {}
        _ => std::future::pending::<()>().await,
    }
}
async fn snapshot_history(
    shared: &Shared,
    events: &[eden_protocol::Event],
) -> Result<Arc<Vec<eden_protocol::coding::Record>>, Fault> {
    let mut cache = shared.history.lock().await;
    let current = events.last().map_or(0, |event| event.sequence);
    let refresh = cache.as_ref().is_none_or(|(previous, _)| {
        events
            .first()
            .is_some_and(|event| event.sequence > previous.saturating_add(1))
            || events.iter().any(|event| {
                event.sequence > *previous
                    && !matches!(
                        event.kind.as_str(),
                        "model_text_delta"
                            | "model_reasoning_delta"
                            | "model_tool_delta"
                            | "model_usage"
                    )
            })
    });
    if refresh {
        let history = eden_protocol::history::active_path(&shared.session.history().await?)?;
        *cache = Some((current, Arc::new(history)));
    }
    if let Some((sequence, history)) = cache.as_mut() {
        *sequence = current;
        Ok(history.clone())
    } else {
        Err(fault("Unavailable", "history snapshot missing"))
    }
}

fn check_session(body: &Value, expected: u64) -> Result<(), Fault> {
    if let Some(value) = body.get("session_id") {
        let id = value
            .as_u64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
            .ok_or_else(|| fault("InvalidInput", "invalid session identity"))?;
        if id != expected {
            return Err(fault(
                "SessionMismatch",
                "endpoint belongs to a different session",
            ));
        }
    }
    Ok(())
}
