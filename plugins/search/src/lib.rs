//! FFF search in a worker process owned by the plugin, including its background threads.
use eden_plugin_sdk::{
    CallContext, Package,
    protocol::{
        Descriptor, Fault,
        coding::{ToolDefinition, ToolRequest, ToolResult},
        resources::{Catalog, CatalogRequest},
    },
    serde_json::{self, Value, json},
};
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
};
mod render;
const CATALOG: &str = "eden.search-catalog.v1";
const EXECUTE: &str = "eden.search-tool.v1";
fn fault(code: &str, message: impl Into<String>) -> Fault {
    Fault::new(code, "search", message)
}
struct ExchangeFailure {
    fault: Fault,
    response_consumed: bool,
}
impl From<Fault> for ExchangeFailure {
    fn from(fault: Fault) -> Self {
        Self {
            fault,
            response_consumed: false,
        }
    }
}
struct Worker {
    process: Mutex<Child>,
    io: Mutex<(ChildStdin, BufReader<ChildStdout>)>,
}
impl Worker {
    fn start(path: &Path) -> Result<Self, Fault> {
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| fault("SearchUnavailable", format!("{}: {e}", path.display())))?;
        let input = child.stdin.take().expect("piped stdin");
        let output = child.stdout.take().expect("piped stdout");
        Ok(Self {
            process: Mutex::new(child),
            io: Mutex::new((input, BufReader::new(output))),
        })
    }
    fn exchange(
        &self,
        request: ToolRequest,
        mut progress: impl FnMut(&str, Value) -> Result<(), Fault>,
    ) -> Result<Value, ExchangeFailure> {
        let mut io = self.io.lock().unwrap_or_else(|e| e.into_inner());
        serde_json::to_writer(&mut io.0, &request)
            .map_err(|e| fault("SearchFailure", e.to_string()))?;
        writeln!(io.0)
            .and_then(|_| io.0.flush())
            .map_err(|e| fault("SearchFailure", e.to_string()))?;
        loop {
            let mut line = String::new();
            if io
                .1
                .read_line(&mut line)
                .map_err(|e| fault("SearchFailure", e.to_string()))?
                == 0
            {
                return Err(fault("SearchFailure", "search worker exited").into());
            }
            let reply: Value =
                serde_json::from_str(&line).map_err(|e| fault("SearchFailure", e.to_string()))?;
            if let Some(stage) = reply["progress"].as_str() {
                progress(
                    &format!("search_{stage}"),
                    serde_json::json!({
                    "worker_pid":reply["worker_pid"]
                    }),
                )?;
                continue;
            }
            if let Some(error) = reply["error"].as_str() {
                return Err(ExchangeFailure {
                    fault: fault("InvalidSearch", error),
                    response_consumed: true,
                });
            }
            return reply
                .get("result")
                .cloned()
                .ok_or_else(|| fault("SearchFailure", "invalid worker response").into());
        }
    }
    fn stop(&self) -> Result<(), Fault> {
        let mut child = self.process.lock().unwrap_or_else(|e| e.into_inner());
        if child
            .try_wait()
            .map_err(|e| fault("CleanupFailure", e.to_string()))?
            .is_none()
        {
            child
                .kill()
                .map_err(|e| fault("CleanupFailure", e.to_string()))?;
        }
        child
            .wait()
            .map_err(|e| fault("CleanupFailure", e.to_string()))?;
        Ok(())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
struct State {
    executable: PathBuf,
    allow_broad_scan: bool,
    excluded_paths: Vec<PathBuf>,
    history_dir: Option<PathBuf>,
    worker: tokio::sync::Mutex<Option<Arc<Worker>>>,
}
impl State {
    async fn invoke(
        self: Arc<Self>,
        mut request: ToolRequest,
        cx: CallContext,
    ) -> Result<ToolResult, Fault> {
        request.arguments["allow_broad_scan"] = json!(self.allow_broad_scan);
        request.arguments["_history_dir"] = json!(self.history_dir);
        request.arguments["_excluded_paths"] = json!(self.excluded_paths);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let cancel = cx.scope.cancellation();
        let progress = cx.clone();
        cx.scope.spawn(async move {
            let result = self
                .exchange_owned(request, cancel, move |kind, payload| {
                    progress.emit(kind, payload)
                })
                .await;
            let cleanup = result
                .as_ref()
                .err()
                .filter(|e| e.code == "CleanupFailure")
                .cloned();
            let _ = sender.send(result);
            match cleanup {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })?;
        receiver
            .await
            .map_err(|_| fault("Cancelled", "search stopped"))?
    }
    async fn exchange_owned(
        self: Arc<Self>,
        request: ToolRequest,
        cancel: eden_plugin_sdk::Cancellation,
        progress: impl FnMut(&str, Value) -> Result<(), Fault> + Send + 'static,
    ) -> Result<ToolResult, Fault> {
        let mut slot = tokio::select! {
            biased;
            _=cancel.cancelled()=>return Err(fault("Cancelled","search stopped")),
            slot=self.worker.lock()=>slot,
        };
        if slot.is_none() {
            *slot = Some(Arc::new(Worker::start(&self.executable)?));
        }
        let worker = slot.as_ref().unwrap().clone();
        let exchange_worker = worker.clone();
        let mut exchange =
            tokio::task::spawn_blocking(move || exchange_worker.exchange(request, progress));
        let result = tokio::select! {
            result=&mut exchange=>result.map_err(|e|ExchangeFailure::from(fault("SearchFailure",e.to_string()))).and_then(|r|r),
            _=cancel.cancelled()=>{
                let stopped=tokio::task::spawn_blocking(move ||worker.stop()).await.map_err(|e|fault("CleanupFailure",e.to_string()))?;
                let _=exchange.await;*slot=None;stopped?;
                return Err(fault("Cancelled","search stopped"));
            }
        };
        // Only a fully read business-error response preserves stream alignment.
        // A rejected progress event can leave the old result queued, regardless
        // of the Fault code returned by the event sink.
        if result.as_ref().is_err_and(|e| !e.response_consumed) {
            let stopped = tokio::task::spawn_blocking(move || worker.stop())
                .await
                .map_err(|e| fault("CleanupFailure", e.to_string()))?;
            *slot = None;
            stopped?;
        }
        result.map_err(|error| error.fault).map(|value| ToolResult {
            text: render::render(&value),
            truncated: false,
            exit_code: None,
            error: None,
        })
    }
}
fn descriptor() -> Descriptor {
    Descriptor {
        package: "search".into(),
        version: "0.1.0".into(),
        provides: vec![
            CATALOG.into(),
            EXECUTE.into(),
            "eden.search-access.v1".into(),
            eden_plugin_sdk::protocol::INSTANCE_STOP.into(),
        ],
    }
}
fn create(config: Value) -> Result<Package, Fault> {
    let executable = match config.get("worker").and_then(Value::as_str) {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()
            .map_err(|e| fault("SearchUnavailable", e.to_string()))?
            .with_file_name(if cfg!(windows) {
                "eden-search-worker.exe"
            } else {
                "eden-search-worker"
            }),
    };
    if !executable.is_absolute() {
        return Err(fault(
            "InvalidInput",
            "search.worker must be an absolute executable path",
        ));
    }
    let state = Arc::new(State {
        executable,
        allow_broad_scan: config["allow_broad_scan"] == true,
        excluded_paths: config.get("excluded_paths").map_or(Ok(vec![]), |value| {
            value
                .as_array()
                .ok_or_else(|| fault("InvalidInput", "excluded_paths must be an array"))?
                .iter()
                .map(|path| {
                    path.as_str()
                        .map(PathBuf::from)
                        .filter(|p| p.is_absolute())
                        .ok_or_else(|| {
                            fault("InvalidInput", "excluded_paths must contain absolute paths")
                        })
                })
                .collect::<Result<_, _>>()
        })?,
        history_dir: if config["persist_history"] == true {
            Some(PathBuf::from(config["history_dir"].as_str().ok_or_else(
                || {
                    fault(
                        "InvalidInput",
                        "history_dir is required for persistent history",
                    )
                },
            )?))
        } else {
            None
        },
        worker: tokio::sync::Mutex::new(None),
    });
    let finalizer = state.clone();
    let observer = state.clone();
    Ok(Package::new("search")
        .service(CATALOG, |_: CatalogRequest, _| async {
            Ok(Catalog { tools: catalog() })
        })
        .service(EXECUTE, move |request: ToolRequest, cx| {
            state.clone().invoke(request, cx)
        })
        .service(
            "eden.search-access.v1",
            move |mut request: ToolRequest, cx| {
                let state = observer.clone();
                async move {
                    if state.history_dir.is_none() {
                        return Ok(());
                    }
                    request.name = "__record_read".into();
                    state.invoke(request, cx).await?;
                    Ok(())
                }
            },
        )
        .service(
            eden_plugin_sdk::protocol::INSTANCE_STOP,
            move |_: Value, _| {
                let state = finalizer.clone();
                async move {
                    if let Some(worker) = state.worker.lock().await.take() {
                        tokio::task::spawn_blocking(move || worker.stop())
                            .await
                            .map_err(|e| fault("CleanupFailure", e.to_string()))??;
                    }
                    Ok(())
                }
            },
        ))
}
eden_plugin_sdk::export_plugin!(descriptor, create);
fn catalog() -> Vec<ToolDefinition> {
    ["find",
 "grep"].into_iter().map(|name| {
        let mut properties = json!({
            "pattern":{
"type":"string",
"minLength":1
},
            "path":{
"type":"string",
"description":"Existing file or directory, relative to cwd or absolute. Default cwd. \
                Never widens a missing path."
},
            "ranking":{
"type":"string",
"enum":if name=="find"{
vec!["relevance",
"git",
"history"]
}else{
vec!["relevance",
"git",
"definition",
"history"]
},
"default":"relevance",
"description":"Explicit optional priority; history requires configured persistence, \
                definition is a heuristic hint."
},
            "case":{
"type":"string",
"enum":["sensitive",
"insensitive",
"smart"],
"default":"sensitive"
},
            "exclude":{
"type":"array",
"items":{
"type":"string"
},
"description":"Explicit relative-path globs"
},
            "limit":{
"type":"integer",
"minimum":1,
"maximum":200,
"default":30
},
            "cursor":{
"type":"string",
"description":"Continue with exactly the same search arguments. Invalidated by file \
                changes, eviction or reopen."
},
            "refresh":{
"type":"boolean",
"description":"Rebuild the in-memory index before a new query"
},
            "follow_symlinks":{
"type":"boolean",
"default":false
}
});
        properties["mode"] = if name == "grep" {
 json!({
"type":"string",
"enum":["literal",
"regex",
"fuzzy"],
"default":"literal"
})
} else {
 json!({
"type":"string",
"enum":["fuzzy",
"glob"],
"default":"fuzzy"
})
};
        if name == "grep" {
            properties["fallback"] = json!({
"type":"string",
"enum":["fuzzy"],
"description":"Opt in only for literal grep: fuzzy candidates after complete zero exact \
                matches, in the same scope. Exact and candidate results remain separate."
});
            properties["max_file_bytes"] = json!({
"type":"integer",
"minimum":1,
"maximum":10485760,
"default":5242880
});
}
        ToolDefinition {
 name:name.into(),
description:if name == "grep" {
 "Search file contents with FFF. Literal and case-sensitive by default. \
                Results are grouped by file with line numbers; has_more means continue \
                using cursor. complete describes evaluated scope, while skipped and \
                line_truncated describe omissions. Fuzzy results are candidates, not proof \
                of exact occurrence."
} else {
 "Find files by fuzzy whole relative path (default) or explicit glob. \
                Returns compact grouped pages with explicit continuation and index state."
}.into(),
parameters:json!({
"type":"object",
"properties":properties,
"required":["pattern"],
"additionalProperties":false
})
}
}).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[tokio::test]
    async fn rejected_progress_discards_unread_response_before_the_next_query() {
        let root =
            std::env::temp_dir().join(format!("eden-search-exchange-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let fixture = Fixture(root);
        let source = fixture.0.join("helper.rs");
        let executable = fixture.0.join(if cfg!(windows) {
            "helper.exe"
        } else {
            "helper"
        });
        std::fs::write(&source, r#"
use std::io::{BufRead, Write};
fn main() {
    let mut output = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        if line.contains("invalid") {
            writeln!(output, "{{\"error\":\"invalid query\"}}").unwrap();
            output.flush().unwrap();
            continue;
        }
        let name = if line.contains("alpha") { "alpha" } else { "beta" };
        if name == "alpha" {
            writeln!(output, "{{\"progress\":\"index_started\",\"worker_pid\":{}}}", std::process::id()).unwrap();
            output.flush().unwrap();
        }
        writeln!(output, "{{\"result\":{{\"mode\":\"{}\",\"index\":{{\"worker_pid\":{}}}}}}}", name, std::process::id()).unwrap();
        output.flush().unwrap();
    }
}
"#).unwrap();
        let compiled = Command::new("rustc")
            .args(["--edition=2024", "--crate-name", "search_exchange_helper"])
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let state = Arc::new(State {
            executable,
            allow_broad_scan: false,
            excluded_paths: vec![],
            history_dir: None,
            worker: tokio::sync::Mutex::new(None),
        });
        let first_pid = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let progress_pid = first_pid.clone();
        let request = |name: &str| ToolRequest {
            cwd: fixture.0.to_string_lossy().into_owned(),
            call_id: name.into(),
            name: "grep".into(),
            arguments: json!({"pattern":name}),
        };
        let failed = state
            .clone()
            .exchange_owned(
                request("alpha"),
                eden_plugin_sdk::Cancellation::default(),
                move |_, payload| {
                    progress_pid.store(
                        payload["worker_pid"].as_u64().unwrap() as u32,
                        std::sync::atomic::Ordering::SeqCst,
                    );
                    Err(fault("Unavailable", "scope is closed"))
                },
            )
            .await
            .unwrap_err();
        assert_eq!(failed.code, "Unavailable");
        let next = state
            .clone()
            .exchange_owned(
                request("beta"),
                eden_plugin_sdk::Cancellation::default(),
                |_, _| Ok(()),
            )
            .await
            .unwrap();
        let header: Value = serde_json::from_str(next.text.lines().next().unwrap()).unwrap();
        let old_pid = first_pid.load(std::sync::atomic::Ordering::SeqCst);
        let invalid = state
            .clone()
            .exchange_owned(
                request("invalid"),
                eden_plugin_sdk::Cancellation::default(),
                |_, _| Ok(()),
            )
            .await
            .unwrap_err();
        assert_eq!(invalid.code, "InvalidSearch");
        let reused = state
            .clone()
            .exchange_owned(
                request("beta"),
                eden_plugin_sdk::Cancellation::default(),
                |_, _| Ok(()),
            )
            .await
            .unwrap();
        let reused: Value = serde_json::from_str(reused.text.lines().next().unwrap()).unwrap();
        assert_eq!(reused["index"]["worker_pid"], header["index"]["worker_pid"]);
        let stopped = state.worker.lock().await.take().unwrap();
        stopped.stop().unwrap();
        assert_eq!(
            header["mode"], "beta",
            "the rejected call's queued result must not escape into the next query"
        );
        assert_ne!(
            old_pid,
            header["index"]["worker_pid"].as_u64().unwrap() as u32
        );
    }
}
