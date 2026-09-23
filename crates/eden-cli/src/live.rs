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
    session: Session,
    token: String,
    web_root: Option<PathBuf>,
    submissions: Mutex<(HashMap<String, Submission>, VecDeque<String>)>,
    stop: watch::Sender<bool>,
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
    let composition = crate::composition(cli)?;
    let options = crate::session_options(cli, cli.session.clone(), true)?;
    let session =
        Session::open_with_workspace(composition, options, crate::prompt_options(cli)).await?;
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
    let (stop, mut stopped) = watch::channel(false);
    let shared = Arc::new(Shared {
        session: session.clone(),
        token: endpoint.token,
        web_root: web_root.map(Path::to_owned),
        submissions: Mutex::new((HashMap::new(), VecDeque::new())),
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
                    let _ = serve(stream, shared).await;
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
async fn serve(mut stream: TcpStream, shared: Arc<Shared>) -> Result<(), Fault> {
    let request = read_request(&mut stream).await;
    let (status, mime, bytes) = match request {
        Ok((method, path, headers, body)) => {
            let route = path.split('?').next().unwrap_or("/");
            if method == "GET" && (route == "/" || route.starts_with("/assets/")) {
                match static_file(&shared, route, &path) {
                    Ok((mime, bytes)) => (200, mime, bytes),
                    Err(error) => json_error(error),
                }
            } else if headers.get("x-eden-token") != Some(&shared.token) {
                json_error(fault("Unauthorized", "invalid live host token"))
            } else {
                match dispatch(&shared, &method, &path, body).await {
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
fn static_file(shared: &Shared, route: &str, path: &str) -> Result<(&'static str, Vec<u8>), Fault> {
    if route == "/" && !path.contains(&format!("token={}", shared.token)) {
        return Err(fault(
            "Unauthorized",
            "open the explicit URL from the endpoint file",
        ));
    }
    let root = shared
        .web_root
        .as_ref()
        .ok_or_else(|| fault("Unavailable", "built web adapter not configured"))?;
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
    let route = path.split('?').next().unwrap_or(path);
    match (method, route) {
        ("POST", "/attach") => {
            let frontend: String = field(&body, "frontend")?;
            Ok(json!({ "attachment": session.attach_presentation(&frontend)? }))
        }
        ("POST", "/detach") => {
            session.detach_presentation(field(&body, "attachment")?)?;
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
        ("POST", "/prompt") => {
            let id: String = field(&body, "request_id")?;
            if id.is_empty() || id.len() > 256 {
                return Err(fault("InvalidInput", "invalid request id"));
            }
            let text: String = field(&body, "text")?;
            let mut guard = shared.submissions.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = guard.0.get(&id) {
                return match existing {
                    Submission::Done {
                        body: prior,
                        result,
                    } if *prior == body => result.clone(),
                    _ => Err(fault(
                        "DuplicateId",
                        "request id reused with different submission",
                    )),
                };
            }
            let result = session.submit(text).map(|run| json!({ "run_id": run }));
            guard.0.insert(
                id.clone(),
                Submission::Done {
                    body,
                    result: result.clone(),
                },
            );
            guard.1.push_back(id);
            while guard.1.len() > 1024 {
                if let Some(id) = guard.1.pop_front() {
                    guard.0.remove(&id);
                }
            }
            result
        }
        ("POST", "/enqueue") => {
            let id: String = field(&body, "request_id")?;
            if id.is_empty() || id.len() > 256 {
                return Err(fault("InvalidInput", "invalid request id"));
            }
            let kind: String = field(&body, "kind")?;
            if !["steering", "follow_up"].contains(&kind.as_str()) {
                return Err(fault("InvalidInput", "choose steering or follow_up"));
            }
            let text: String = field(&body, "text")?;
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let start = {
                let mut guard = shared.submissions.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(existing) = guard.0.get_mut(&id) {
                    match existing {
                        Submission::Done {
                            body: prior,
                            result,
                        } => {
                            return if *prior == body {
                                result.clone()
                            } else {
                                Err(fault(
                                    "DuplicateId",
                                    "request id reused with different submission",
                                ))
                            };
                        }
                        Submission::Running {
                            body: prior,
                            listeners,
                        } => {
                            if *prior != body {
                                return Err(fault(
                                    "DuplicateId",
                                    "request id reused with different submission",
                                ));
                            }
                            listeners.push(sender);
                            false
                        }
                    }
                } else {
                    guard.0.insert(
                        id.clone(),
                        Submission::Running {
                            body: body.clone(),
                            listeners: vec![sender],
                        },
                    );
                    true
                }
            };
            if start {
                let shared = shared.clone();
                tokio::spawn(async move {
                    let result = shared
                        .session
                        .enqueue(&kind, vec![eden_protocol::coding::Block::Text { text }])
                        .await
                        .map(|entries| json!(entries));
                    let mut guard = shared.submissions.lock().unwrap_or_else(|e| e.into_inner());
                    let listeners = match guard.0.remove(&id) {
                        Some(Submission::Running { listeners, .. }) => listeners,
                        _ => vec![],
                    };
                    for listener in listeners {
                        let _ = listener.send(result.clone());
                    }
                    guard
                        .0
                        .insert(id.clone(), Submission::Done { body, result });
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
                .map_err(|_| fault("Unavailable", "queue result lost"))?
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
/// A small HTTP client for the terminal adapter and installed process probe.
pub async fn call(
    endpoint_path: &Path,
    method: &str,
    route: &str,
    body: Option<&Value>,
) -> Result<Value, Fault> {
    let endpoint: Endpoint = serde_json::from_slice(
        &std::fs::read(endpoint_path).map_err(|error| fault("FileFailure", error.to_string()))?,
    )
    .map_err(|error| fault("InvalidInput", error.to_string()))?;
    let mut stream = TcpStream::connect(&endpoint.address)
        .await
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
    stream
        .write_all(header.as_bytes())
        .await
        .map_err(|error| fault("OutputFailure", error.to_string()))?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|error| fault("OutputFailure", error.to_string()))?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .map_err(|error| fault("InputFailure", error.to_string()))?;
    let start = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| fault("InvalidInput", "invalid response"))?
        + 4;
    let envelope: Value = serde_json::from_slice(&response[start..])
        .map_err(|error| fault("InvalidInput", error.to_string()))?;
    if envelope["ok"] == true {
        Ok(envelope["result"].clone())
    } else {
        Err(serde_json::from_value(envelope["error"].clone())
            .map_err(|error| fault("InvalidInput", error.to_string()))?)
    }
}
/// Start the terminal adapter against one explicitly selected host.
pub async fn run_tui(endpoint_path: &Path) -> Result<i32, Box<dyn std::error::Error>> {
    crate::live_tui::run(endpoint_path).await
}
