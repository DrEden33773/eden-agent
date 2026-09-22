//! Tool scheduling keeps parallel work inside the run's managed scope.
use super::*;

struct Pending {
    call_id: String,
    result: tokio::sync::oneshot::Receiver<Result<ToolResult, Fault>>,
}

async fn prepare(
    cx: &CallContext,
    cwd: &str,
    call_id: &str,
    name: String,
    arguments: String,
) -> Result<ToolRequest, Fault> {
    let original = ToolRequest {
        cwd: cwd.into(),
        call_id: call_id.into(),
        name,
        arguments: serde_json_decode(&arguments)?,
    };
    let execution = optional_call::<_, ToolRequest>(cx, r::BEFORE_TOOL, &original)
        .await?
        .unwrap_or_else(|| original.clone());
    if execution.call_id != original.call_id || execution.cwd != original.cwd {
        return Err(Fault::new(
            "InvalidInput",
            "before-tool",
            "hooks may change tool name and arguments, but not call identity or cwd",
        ));
    }
    append(
        cx,
        "tool_execution_intent",
        json!({ "original": original, "execution": execution }),
    )
    .await?;
    Ok(execution)
}

fn start(cx: &CallContext, request: ToolRequest) -> Result<Pending, Fault> {
    let call_id = request.call_id.clone();
    let (sender, result) = tokio::sync::oneshot::channel();
    let worker = cx.clone();
    // The owner survives root cancellation and drains its service bridge before
    // the run can settle; dropping a result receiver never detaches tool work.
    cx.scope.spawn(async move {
        let result = worker.call::<_, ToolResult>(TOOL, &request).await;
        if let Err(Err(error)) = sender.send(result)
            && error.code == "CleanupFailure"
        {
            return Err(error);
        }
        Ok(())
    })?;
    Ok(Pending { call_id, result })
}

async fn commit(
    cx: &CallContext,
    call_id: String,
    result: Result<ToolResult, Fault>,
) -> Result<(), Fault> {
    let result = match result {
        Ok(value) => value,
        Err(error) if error.code == "Cancelled" || error.code == "CleanupFailure" => {
            return Err(error);
        }
        Err(error) => ToolResult {
            content: vec![],
            details: Value::Null,
            artifacts: vec![],
            text: error.to_string(),
            exit_code: None,
            truncated: false,
            error: Some(error),
        },
    };
    append(
        cx,
        "tool_result",
        json!(Item::ToolResult {
            call_id: call_id.clone(),
            result
        }),
    )
    .await
    .map_err(|error| {
        Fault::new(
            "PersistenceFailure",
            "coding-loop",
            format!(
                "Tool call {call_id}: external side effects may already have occurred; result \
                 commit failed: {error}"
            ),
        )
    })?;
    Ok(())
}

async fn flush(cx: &CallContext, pending: &mut Vec<Pending>) -> Result<(), Fault> {
    for Pending { call_id, result } in std::mem::take(pending) {
        let result = result
            .await
            .map_err(|_| Fault::new("Unavailable", "coding-loop", "tool completion lost"))?;
        commit(cx, call_id, result).await?;
    }
    Ok(())
}

pub(super) async fn run(
    cx: &CallContext,
    cwd: &str,
    items: Vec<Item>,
    tools: &[ToolDefinition],
) -> Result<(bool, String), Fault> {
    let mut pending = vec![];
    let mut called = false;
    let mut answer = String::new();
    for item in items {
        match item {
            Item::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                called = true;
                let request = prepare(cx, cwd, &call_id, name, arguments).await;
                let parallel = request.as_ref().is_ok_and(|request| {
                    tools.iter().any(|tool| {
                        tool.name == request.name && tool.execution == ToolExecution::Parallel
                    })
                });
                if !parallel {
                    flush(cx, &mut pending).await?;
                }
                match request {
                    Ok(request) if parallel => pending.push(start(cx, request)?),
                    Ok(request) => commit(cx, call_id, cx.call(TOOL, &request).await).await?,
                    Err(error) => commit(cx, call_id, Err(error)).await?,
                }
            }
            other => {
                flush(cx, &mut pending).await?;
                if let Item::Message { content, .. } = other {
                    for block in content {
                        if let Block::Text { text } = block {
                            answer.push_str(&text);
                        }
                    }
                }
            }
        }
    }
    flush(cx, &mut pending).await?;
    Ok((called, answer))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_tool_schemas_keep_sequential_execution() {
        let tool: ToolDefinition = serde_json::from_value(json!({
            "name": "old",
            "description": "legacy",
            "parameters": {},
        }))
        .unwrap();
        assert_eq!(tool.execution, ToolExecution::Sequential);
        let parallel: ToolDefinition = serde_json::from_value(json!({
            "name": "new",
            "description": "parallel",
            "parameters": {},
            "execution": "parallel",
        }))
        .unwrap();
        assert_eq!(parallel.execution, ToolExecution::Parallel);
    }

    use eden_plugin_sdk::{
        Cancellation,
        abi::{Bytes, HostApi, Reply},
        local::LocalInstance,
        protocol::{Outcome, Request, Terminal},
    };
    use std::sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    };
    use tokio::sync::mpsc;
    enum Observation {
        Tool(u64, ToolRequest, Reply),
        Cancel(u64),
    }
    struct Host {
        records: Mutex<Vec<Record>>,
        next: AtomicU64,
        observed: mpsc::UnboundedSender<Observation>,
    }
    unsafe extern "C" fn request(context: usize, bytes: Bytes, reply: Reply) -> u64 {
        // SAFETY: Tests retain the boxed host through the instance stop barrier.
        let host = unsafe { &*(context as *const Host) };
        // SAFETY: The SDK borrows bytes for the duration of this callback.
        let request: Request = unsafe { bytes.decode() }.unwrap();
        let id = host.next.fetch_add(1, Ordering::Relaxed);
        let result = match request.contract.as_str() {
            TOOL => {
                host.observed
                    .send(Observation::Tool(
                        id,
                        serde_json::from_value(request.payload).unwrap(),
                        reply,
                    ))
                    .unwrap();
                return id;
            }
            r::BEFORE_TOOL => {
                let mut tool: ToolRequest = serde_json::from_value(request.payload).unwrap();
                if tool.call_id == "a" {
                    tool.name = "parallel".into();
                }
                if tool.call_id == "c" {
                    tool.name = "sequential".into();
                }
                json!(tool)
            }
            STORE => {
                let mut records = host.records.lock().unwrap();
                let StoreRequest::Append {
                    kind,
                    payload,
                    run_id,
                } = serde_json::from_value(request.payload).unwrap()
                else {
                    panic!("expected append");
                };
                let sequence = records.len() as u64 + 1;
                records.push(Record {
                    parent_id: sequence.checked_sub(1).filter(|n| *n > 0),
                    branch: "main".into(),
                    schema_version: 2,
                    session_id: 1,
                    sequence,
                    run_id,
                    kind,
                    payload,
                });
                json!(StoreReply {
                    active_head: Some(sequence),
                    active_branch: "main".into(),
                    session_id: 1,
                    sequence,
                    records: vec![]
                })
            }
            other => panic!("unexpected contract {other}"),
        };
        // SAFETY: Each accepted request consumes its unique reply token once.
        unsafe {
            reply.send(&Terminal {
                outcome: Outcome::Completed(result),
                cleanup_errors: vec![],
            })
        };
        id
    }
    unsafe extern "C" fn cancel(context: usize, id: u64) {
        // SAFETY: Host state outlives all callbacks and shares only synchronized data.
        let host = unsafe { &*(context as *const Host) };
        host.observed.send(Observation::Cancel(id)).unwrap();
    }
    unsafe extern "C" fn event(_: usize, _: Bytes) {}
    fn fixture() -> (
        Box<Host>,
        Arc<LocalInstance>,
        mpsc::UnboundedReceiver<Observation>,
    ) {
        let (observed, receiver) = mpsc::unbounded_channel();
        let host = Box::new(Host {
            records: Mutex::new(vec![]),
            next: AtomicU64::new(1),
            observed,
        });
        let tools: Vec<ToolDefinition> = serde_json::from_value(json!([
            {
                "name": "parallel",
                "description": "parallel",
                "parameters": {},
                "execution": "parallel",
            },
            { "name": "sequential", "description": "sequential", "parameters": {} },
        ]))
        .unwrap();
        let package =
            Package::new("fixture").service("test.execute", move |items: Vec<Item>, cx| {
                let tools = tools.clone();
                async move { run(&cx, "/session", items, &tools).await }
            });
        // SAFETY: Tests keep host alive until stop; request/event copy borrowed
        // spans synchronously and deferred replies remain uniquely owned by tests.
        let instance = unsafe {
            LocalInstance::new(
                package,
                HostApi {
                    context: &*host as *const Host as usize,
                    request,
                    cancel,
                    event,
                },
            )
        };
        (host, instance, receiver)
    }
    fn call(id: &str, name: &str) -> Item {
        Item::ToolCall {
            call_id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
        }
    }
    fn round(items: Vec<Item>) -> Request {
        Request {
            session_id: 1,
            run_id: 1,
            contract: "test.execute".into(),
            payload: json!(items),
        }
    }
    async fn next(receiver: &mut mpsc::UnboundedReceiver<Observation>) -> Observation {
        tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap()
    }
    fn complete(reply: Reply, value: &str) {
        let result = ToolResult {
            content: vec![],
            details: Value::Null,
            artifacts: vec![],
            text: value.into(),
            exit_code: None,
            truncated: false,
            error: None,
        };
        // SAFETY: The test owns the accepted tool's single-use reply until this call.
        unsafe {
            reply.send(&Terminal {
                outcome: Outcome::Completed(json!(result)),
                cleanup_errors: vec![],
            })
        };
    }
    fn results(host: &Host) -> Vec<String> {
        host.records
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.kind == "tool_result")
            .map(|r| r.payload["call_id"].as_str().unwrap().to_owned())
            .collect()
    }
    #[tokio::test]
    async fn final_hook_target_controls_parallel_batches_and_sequential_barriers() {
        let (host, instance, mut observations) = fixture();
        let client = instance.clone();
        let task = tokio::spawn(async move {
            client
                .call(
                    round(vec![
                        call("a", "sequential"),
                        call("b", "parallel"),
                        call("c", "parallel"),
                        call("d", "parallel"),
                    ]),
                    Cancellation::default(),
                )
                .await
        });
        let mut first = vec![next(&mut observations).await, next(&mut observations).await];
        let mut replies = std::collections::BTreeMap::new();
        for item in first.drain(..) {
            let Observation::Tool(_, request, reply) = item else {
                panic!("tool expected");
            };
            assert_eq!(request.name, "parallel");
            replies.insert(request.call_id, reply);
        }
        complete(replies.remove("b").unwrap(), "second finishes first");
        assert!(results(&host).is_empty());
        assert!(observations.try_recv().is_err());
        complete(replies.remove("a").unwrap(), "first finishes second");
        let Observation::Tool(_, request, reply) = next(&mut observations).await else {
            panic!("sequential tool expected");
        };
        assert_eq!(request.call_id, "c");
        assert_eq!(request.name, "sequential");
        assert_eq!(results(&host), ["a", "b"]);
        assert!(observations.try_recv().is_err());
        complete(reply, "barrier");
        let Observation::Tool(_, request, reply) = next(&mut observations).await else {
            panic!("last tool expected");
        };
        assert_eq!(request.call_id, "d");
        assert_eq!(results(&host), ["a", "b", "c"]);
        complete(reply, "last");
        task.await.unwrap().into_result().unwrap();
        assert_eq!(results(&host), ["a", "b", "c", "d"]);
        instance.stop().await.unwrap();
    }
    #[tokio::test]
    async fn cancelling_parallel_batch_keeps_all_bridges_owned_until_completion() {
        let (host, instance, mut observations) = fixture();
        let client = instance.clone();
        let cancellation = Cancellation::default();
        let token = cancellation.clone();
        let task = tokio::spawn(async move {
            client
                .call(
                    round(vec![call("a", "parallel"), call("b", "parallel")]),
                    token,
                )
                .await
        });
        let mut replies = std::collections::BTreeMap::new();
        for _ in 0..2 {
            let Observation::Tool(id, _, reply) = next(&mut observations).await else {
                panic!("tool expected");
            };
            replies.insert(id, reply);
        }
        cancellation.cancel();
        for _ in 0..2 {
            let Observation::Cancel(id) = next(&mut observations).await else {
                panic!("cancellation expected");
            };
            assert!(replies.contains_key(&id));
        }
        assert!(
            !task.is_finished(),
            "the run must wait for both tool bridges"
        );
        for reply in replies.into_values() {
            complete(reply, "settled after cancellation");
        }
        let terminal = task.await.unwrap();
        assert!(matches!(terminal.outcome, Outcome::Cancelled));
        assert!(terminal.cleanup_errors.is_empty());
        assert!(results(&host).is_empty());
        instance.stop().await.unwrap();
    }
}
