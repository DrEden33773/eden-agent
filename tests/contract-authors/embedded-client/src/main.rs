//! Installed SDK consumer: native default loop invokes caller-runtime tools and frozen resources.
use eden_agent::{
    SessionOptions, WorkspaceOptions,
    embedded::{Embedded, ToolOptions},
};
use eden_plugin_sdk::{Package, protocol as eden_protocol, serde_json, tokio};
use eden_protocol::{Composition, Fault, coding as c, models as m, resources as r};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

fn embedding(
    composition: Composition,
    base: PathBuf,
    fail_reload: Arc<AtomicBool>,
) -> Result<Embedded, Fault> {
    let provider =
        Package::new("probe-provider").service(c::PROVIDER, |input: c::ModelInput, _| async move {
            if !input.tools.iter().any(|tool| tool.name == "host_marker") {
                return Err(Fault::new(
                    "ProbeFailure",
                    "provider",
                    "injected schema missing",
                ));
            }
            if !serde_json::to_string(&input.items)
                .unwrap_or_default()
                .contains("HOST_SYSTEM")
            {
                return Err(Fault::new(
                    "ProbeFailure",
                    "provider",
                    "injected system missing",
                ));
            }
            let cancel_test = serde_json::to_string(&input.items)
                .unwrap_or_default()
                .contains("CANCEL_HOST");
            let result = input.items.iter().rev().find_map(|item| match item {
                c::Item::ToolResult { result, .. } => Some(result),
                _ => None,
            });
            Ok(c::ModelReply {
                usage: Value::Null,
                items: if let Some(result) = result {
                    vec![c::Item::Message {
                        role: "assistant".into(),
                        content: vec![c::Block::Text {
                            text: result.text.clone(),
                        }],
                    }]
                } else {
                    vec![c::Item::ToolCall {
                        call_id: "probe-call".into(),
                        name: "host_marker".into(),
                        arguments: json!({
                            "text": if cancel_test {
                                    "wait"
                                } else {
                                    "from caller"
                                },
                        })
                        .to_string(),
                    }]
                },
            })
        });
    Embedded::new(composition, base.clone())
        .package(provider, "probe-provider-v1")?
        .tool(
            serde_json::from_value(json!({
                "name": "host_marker",
                "description": "Write a marker in this session directory",
                "parameters": {
                    "type": "object",
                    "required": ["text"],
                    "properties": { "text": { "type": "string" } },
                    "additionalProperties": false,
                },
            }))
            .map_err(|e| Fault::new("InvalidInput", "probe", e.to_string()))?,
            "host-marker-v1",
            ToolOptions::default(),
            Ok,
            |request, cx| async move {
                cx.emit(
                    "host_tool_delta",
                    json!({ "call_id": request.call_id, "text": "writing" }),
                )?;
                let text = request.arguments["text"]
                    .as_str()
                    .ok_or_else(|| Fault::new("InvalidInput", "probe", "text missing"))?;
                if text == "wait" {
                    let marker = Path::new(&request.cwd).join("cancelled-tool-cleanup");
                    cx.scope.cleanup(async move {
                        std::fs::write(marker, b"settled")
                            .map_err(|e| Fault::new("CleanupFailure", "probe", e.to_string()))
                    })?;
                    cx.emit("host_tool_waiting", json!({}))?;
                    return std::future::pending::<Result<c::ToolResult, Fault>>().await;
                }
                std::fs::write(Path::new(&request.cwd).join("marker.txt"), text)
                    .map_err(|e| Fault::new("FileFailure", "probe", e.to_string()))?;
                Ok(c::ToolResult {
                    content: vec![],
                    details: json!({ "cwd": request.cwd }),
                    artifacts: vec![],
                    text: text.into(),
                    exit_code: None,
                    truncated: false,
                    error: None,
                })
            },
        )?
        .resources("host-resources-v1", base, move |request, _| {
            let fail = fail_reload.load(Ordering::Acquire);
            async move {
                if fail && matches!(request, r::ResourceRequest::Reload) {
                    return Err(Fault::new(
                        "ProbeFailure",
                        "resources",
                        "controlled reload failure",
                    ));
                }
                Ok(r::ResourceReply {
                    snapshot: r::Snapshot {
                        revision: 1,
                        system: Some("HOST_SYSTEM".into()),
                        instructions: "HOST_CONTEXT".into(),
                        sources: vec!["memory/context".into()],
                        ..r::Snapshot::default()
                    },
                    text: None,
                })
            }
        })
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let composition_path = PathBuf::from(
        args.get(1)
            .ok_or("usage: entrypoints_probe COMPOSITION ROOT")?,
    );
    let root = std::fs::canonicalize(args.get(2).ok_or("missing root")?)?;
    let mut composition: Composition = serde_json::from_slice(&std::fs::read(&composition_path)?)?;
    composition.roles.remove(m::MODEL_CATALOG);
    composition.roles.remove(c::MODEL_INFO);
    let base = composition_path
        .parent()
        .ok_or("missing composition base")?
        .to_owned();
    let failed = Arc::new(AtomicBool::new(false));
    let workspace = WorkspaceOptions {
        global_dir: root.join("global"),
        project_trust: Some(false),
        ..WorkspaceOptions::default()
    };
    let cwd = root.join("first");
    let second_cwd = root.join("second");
    std::fs::create_dir_all(&cwd)?;
    std::fs::create_dir_all(&second_cwd)?;
    let history = root.join("history.jsonl");
    let options = SessionOptions {
        cwd: cwd.clone(),
        history: Some(history.clone()),
    };
    let first = embedding(composition.clone(), base.clone(), failed.clone())?
        .open(options.clone(), workspace.clone())
        .await?;
    let second = embedding(composition.clone(), base.clone(), failed.clone())?
        .open(
            SessionOptions {
                cwd: second_cwd.clone(),
                history: None,
            },
            workspace.clone(),
        )
        .await?;
    let run1 = first.submit("write marker")?;
    let run2 = second.submit("write marker")?;
    first.wait(run1).await?.into_result()?;
    second.wait(run2).await?.into_result()?;
    assert_eq!(
        std::fs::read_to_string(cwd.join("marker.txt"))?,
        "from caller"
    );
    assert_eq!(
        std::fs::read_to_string(second_cwd.join("marker.txt"))?,
        "from caller"
    );
    assert_ne!(first.id(), second.id());
    let cancel_dir = root.join("cancel");
    std::fs::create_dir_all(&cancel_dir)?;
    let cancelled = embedding(composition.clone(), base.clone(), failed.clone())?
        .open(
            SessionOptions {
                cwd: cancel_dir.clone(),
                history: None,
            },
            workspace.clone(),
        )
        .await?;
    let cancel_run = cancelled.submit("CANCEL_HOST")?;
    let mut cursor = 0;
    loop {
        let events = cancelled.read_events(cursor).await?;
        cursor = events.last().ok_or("unexpected stream end")?.sequence;
        if events.iter().any(|e| e.kind == "host_tool_waiting") {
            break;
        }
    }
    cancelled.cancel(cancel_run)?;
    assert_eq!(
        cancelled
            .wait(cancel_run)
            .await?
            .into_result()
            .unwrap_err()
            .code,
        "Cancelled"
    );
    assert_eq!(
        std::fs::read(cancel_dir.join("cancelled-tool-cleanup"))?,
        b"settled"
    );
    cancelled.shutdown().await?;

    let resource = first.resources().await?;
    failed.store(true, Ordering::Release);
    let reload = first.reload_resources()?;
    assert!(first.wait(reload).await?.into_result().is_err());
    assert_eq!(first.resources().await?.revision, resource.revision);
    let entry = first
        .enqueue(
            "steering",
            vec![c::Block::Text {
                text: "withdraw me".into(),
            }],
        )
        .await?;
    assert_eq!(first.withdraw_queue(Some(vec![entry.id])).await?.len(), 1);
    first.shutdown().await?;
    second.shutdown().await?;
    failed.store(false, Ordering::Release);
    let reopened = embedding(composition, base, failed)?
        .open(options, workspace)
        .await?;
    assert!(reopened.queued().await?.is_empty());
    assert!(
        reopened
            .history()
            .await?
            .iter()
            .any(|r| r.kind == "queue_withdrawn")
    );
    reopened.shutdown().await?;
    assert!(
        eden_agent::Session::open_with(
            &composition_path,
            SessionOptions {
                cwd,
                history: Some(history.clone())
            }
        )
        .await
        .is_err()
    );
    assert!(!eden_agent::history::read(&history)?.is_empty());
    println!(
        "{}",
        json!({
            "native_default_loop_to_host_tool": true,
            "resource_freeze_and_failed_reload": true,
            "isolated_cwd": true,
            "persistent_withdraw_reopen": true,
            "missing_provider_history_readable": true,
        })
    );
    Ok(())
}
