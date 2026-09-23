//! Independent native author exercising the portable live presentation and action bridge.
use eden_plugin_sdk::{
    Package,
    protocol::{
        AGENT_LOOP, CONTEXT, Descriptor, Fault, PROVIDER, RunInput, TOOL,
        interaction::{HOST as LEGACY_HOST, Interaction},
        presentation::*,
    },
    serde_json::{Value, json},
    tokio,
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

fn descriptor() -> Descriptor {
    Descriptor {
        package: "presentation-live".into(),
        version: "0.1.0".into(),
        provides: vec![
            AGENT_LOOP.into(),
            ACTION.into(),
            CONTEXT.into(),
            PROVIDER.into(),
            TOOL.into(),
        ],
    }
}
fn view(status: &str) -> View {
    let mut view = View::new("review", Slot::Panel, "External tool review")
        .node(Node::Text {
            id: "summary".into(),
            text: "The external Rust tool produced a structured result.".into(),
        })
        .node(Node::Table {
            id: "table".into(),
            columns: vec!["File".into(), "Change".into()],
            rows: vec![vec!["src/main.rs".into(), "updated".into()]],
        })
        .node(Node::Diff {
            id: "diff".into(),
            before: "fn old() {}".into(),
            after: "fn new() {}".into(),
        })
        .node(Node::Attachment {
            id: "attachment".into(),
            name: "review.pdf".into(),
            record_sequence: 1,
        })
        .node(Node::Form {
            id: "decision".into(),
            action: "approve".into(),
            fields: vec![
                Field::text("reason", "Reason", true),
                Field::multi_choice(
                    "checks",
                    "Checks",
                    [
                        "tests", "docs", "lint", "format", "license", "windows", "macos", "linux",
                        "security", "release",
                    ]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                    true,
                ),
            ],
        })
        .node(Node::Status {
            id: "progress".into(),
            text: status.into(),
        });
    view.source = Some(Source {
        record_sequence: None,
        class: ContentClass::Tool,
    });
    view
}
fn create(_: Value) -> Result<Package, Fault> {
    let (sender, _) = tokio::sync::watch::channel(None::<Value>);
    let root_sender = Arc::new(sender);
    let action_sender = root_sender.clone();
    let calls = Arc::new(AtomicU64::new(0));
    Ok(Package::new("presentation-live")
        .service(AGENT_LOOP, move |input: RunInput, cx| {
            let mut answered = root_sender.subscribe();
            async move {
                if input.prompt == "legacy-after-detach" {
                    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                    return cx
                        .call::<_, Value>(
                            LEGACY_HOST,
                            &Interaction::Request {
                                kind: "confirm".into(),
                                title: "Continue detached task?".into(),
                                options: vec![],
                                initial: String::new(),
                                timeout_ms: Some(5000),
                            },
                        )
                        .await;
                }
                cx.present(view("Collecting checks…")).await?;
                let mut native =
                    View::new("terminal-helper", Slot::Panel, "Terminal helper").node(Node::Text {
                        id: "instructions".into(),
                        text: "Terminal-only helper".into(),
                    });
                native.platforms = vec!["tui".into()];
                native.fallback = "Terminal helper is available in the TUI".into();
                native.source = Some(Source {
                    record_sequence: None,
                    class: ContentClass::Extension,
                });
                cx.present(native).await?;
                tokio::time::sleep(std::time::Duration::from_millis(450)).await;
                cx.present(view("Checks ready")).await?;
                cx.emit(
                    "presentation_stream_complete",
                    json!({ "view_id": "review" }),
                )?;
                let cancelled = cx.scope.cancellation();
                tokio::select! {
                    result = answered.changed() => {
                        result.map_err(|_| {
                            Fault::new("Unavailable", "presentation-live", "answer channel closed")
                        })?;
                        let answer = answered.borrow().clone().unwrap_or(Value::Null);
                        cx.emit("presentation_answered", answer.clone())?;
                        Ok::<_, Fault>(answer)
                    },
                    _ = cancelled.cancelled() => Err(Fault::new(
                        "Cancelled",
                        "presentation-live",
                        "run cancelled"
                    )),
                }
            }
        })
        .service(ACTION, move |request: ActionRequest, cx| {
            let sender = action_sender.clone();
            let calls = calls.clone();
            async move {
                if request.action != "approve" {
                    return Err(Fault::new(
                        "InvalidInput",
                        "presentation-live",
                        "unknown action",
                    ));
                }
                cx.emit(
                    "presentation_action_called",
                    json!({ "request_id": request.request_id }),
                )?;
                sender.send_replace(Some(request.values.clone()));
                Ok::<_, Fault>(json!({
                    "accepted": request.values,
                    "calls": calls.fetch_add(1, Ordering::SeqCst) + 1,
                }))
            }
        })
        .service(CONTEXT, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
        .service(PROVIDER, |_: Value, _| async {
            Ok::<_, Fault>(Value::Null)
        })
        .service(TOOL, |_: Value, _| async { Ok::<_, Fault>(Value::Null) }))
}
eden_plugin_sdk::export_plugin!(descriptor, create);
