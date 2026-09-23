//! Live presentation behavior at the public Session boundary.
use eden_agent::{SessionOptions, WorkspaceOptions, embedded::Embedded};
use eden_plugin_sdk::Package;
use eden_protocol::{
    AGENT_LOOP, CONTEXT, Composition, Fault, PROVIDER, RunInput, TOOL, presentation::*,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn composition() -> Composition {
    Composition {
        packages: vec![],
        roles: BTreeMap::new(),
        resource_packages: vec![],
    }
}

async fn session(package: Package) -> eden_agent::Session {
    let cwd = std::env::current_dir().unwrap();
    Embedded::new(composition(), cwd.clone())
        .package(
            package
                .service(CONTEXT, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
                .service(PROVIDER, |_: Value, _| async {
                    Ok::<_, Fault>(Value::Null)
                })
                .service(TOOL, |_: Value, _| async { Ok::<_, Fault>(Value::Null) }),
            "presentation-test-v1",
        )
        .unwrap()
        .open(
            SessionOptions { cwd, history: None },
            WorkspaceOptions {
                global_dir: std::env::temp_dir()
                    .join(format!("eden-presentation-test-{}", std::process::id())),
                project_trust: Some(false),
                ..WorkspaceOptions::default()
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn snapshot_reconnect_and_action_admission_use_one_live_owner() {
    let package = Package::new("presenter")
        .service(AGENT_LOOP, |_: RunInput, cx| async move {
            let view = View::new("tool-result", Slot::Panel, "Review change")
                .node(Node::Diff {
                    id: "diff".into(),
                    before: "old".into(),
                    after: "new".into(),
                })
                .node(Node::Form {
                    id: "review".into(),
                    action: "approve".into(),
                    fields: vec![Field::text("reason", "Reason", true)],
                });
            let _: Revision = cx.present(view).await?;
            std::future::pending::<Result<Value, Fault>>().await
        })
        .service(ACTION, |request: ActionRequest, _| async move {
            Ok::<_, Fault>(json!({ "accepted": request.values["reason"] }))
        });
    let session = session(package).await;
    let first = session.attach_presentation("tui").unwrap();
    let run = session.submit("show").unwrap();
    let snapshot = loop {
        let snapshot = session.presentation_snapshot();
        if !snapshot.views.is_empty() {
            break snapshot;
        }
        session.presentation_changed(snapshot.sequence).await;
    };
    assert_eq!(snapshot.views[0].owner, "presenter");
    session.detach_presentation(first).unwrap();
    assert_eq!(session.state().active_run, Some(run));
    let second = session.attach_presentation("web").unwrap();
    assert_eq!(session.presentation_snapshot().views, snapshot.views);
    let action = ActionRequest {
        session_id: session.id(),
        owner: "presenter".into(),
        view_id: "tool-result".into(),
        revision: snapshot.views[0].revision,
        action: "approve".into(),
        request_id: "web-1".into(),
        values: json!({ "reason": "looks good" }),
    };
    assert_eq!(
        session.presentation_action(action.clone()).await.unwrap(),
        json!({ "accepted": "looks good" })
    );
    assert_eq!(
        session.presentation_action(action.clone()).await.unwrap(),
        json!({ "accepted": "looks good" })
    );
    assert_eq!(
        session
            .presentation_action(ActionRequest {
                request_id: "web-2".into(),
                revision: 0,
                ..action
            })
            .await
            .unwrap_err()
            .code,
        "StaleRevision"
    );
    session.detach_presentation(second).unwrap();
    session.cancel(run).unwrap();
    session.wait(run).await.unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn activity_is_transient_and_never_contains_a_draft() {
    let package = Package::new("presenter").service(AGENT_LOOP, |_: RunInput, _| async {
        Ok::<_, Fault>(Value::Null)
    });
    let session = session(package).await;
    let tui = session.attach_presentation("tui").unwrap();
    let web = session.attach_presentation("web").unwrap();
    session
        .presentation_activity(tui, ActivityTarget::Composer, true)
        .unwrap();
    let snapshot = session.presentation_snapshot();
    assert_eq!(snapshot.activity.len(), 1);
    assert!(!serde_json::to_string(&snapshot).unwrap().contains("draft"));
    session.detach_presentation(tui).unwrap();
    assert!(session.presentation_snapshot().activity.is_empty());
    session.detach_presentation(web).unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn legacy_dialog_survives_frontend_detach_and_uses_the_same_action_validation() {
    let package = Package::new("legacy").service(AGENT_LOOP, |_: RunInput, cx| async move {
        cx.call::<_, Value>(
            eden_protocol::interaction::HOST,
            &eden_protocol::interaction::Interaction::Request {
                kind: "confirm".into(),
                title: "Continue?".into(),
                options: vec![],
                initial: String::new(),
                timeout_ms: Some(2000),
            },
        )
        .await
    });
    let session = session(package).await;
    session.set_interactions(true);
    let first = session.attach_presentation("tui").unwrap();
    let run = session.submit("ask").unwrap();
    let view = loop {
        let snapshot = session.presentation_snapshot();
        if let Some(view) = snapshot
            .views
            .iter()
            .find(|view| view.owner == "eden-host-interaction")
        {
            break view.clone();
        }
        session.presentation_changed(snapshot.sequence).await;
    };
    session.detach_presentation(first).unwrap();
    let next = session.attach_presentation("web").unwrap();
    assert!(session.presentation_snapshot().pending_interactions.len() == 1);
    let request = ActionRequest {
        session_id: session.id(),
        owner: view.owner,
        view_id: view.view.id,
        revision: view.revision,
        action: "respond".into(),
        request_id: "answer-1".into(),
        values: json!({ "value": true }),
    };
    assert_eq!(
        session.presentation_action(request.clone()).await.unwrap(),
        json!({ "delivered": true })
    );
    assert_eq!(
        session.presentation_action(request).await.unwrap(),
        json!({ "delivered": true })
    );
    assert_eq!(
        session.wait(run).await.unwrap().into_result().unwrap(),
        json!(true)
    );
    session.detach_presentation(next).unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_disconnected_action_caller_can_retry_without_executing_twice() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let action_calls = calls.clone();
    let action_entered = entered.clone();
    let action_release = release.clone();
    let package = Package::new("presenter")
        .service(AGENT_LOOP, |_: RunInput, cx| async move {
            cx.present(
                View::new("question", Slot::Panel, "Question").node(Node::Form {
                    id: "answer".into(),
                    action: "submit".into(),
                    fields: vec![Field::text("reason", "Reason", true)],
                }),
            )
            .await?;
            std::future::pending::<Result<Value, Fault>>().await
        })
        .service(ACTION, move |_: ActionRequest, _| {
            let calls = action_calls.clone();
            let entered = action_entered.clone();
            let release = action_release.clone();
            async move {
                let count = calls.fetch_add(1, Ordering::SeqCst) + 1;
                entered.notify_one();
                release.notified().await;
                Ok::<_, Fault>(json!({ "calls": count }))
            }
        });
    let session = session(package).await;
    let attachment = session.attach_presentation("web").unwrap();
    let run = session.submit("ask").unwrap();
    let view = loop {
        let snapshot = session.presentation_snapshot();
        if let Some(view) = snapshot.views.first() {
            break view.clone();
        }
        session.presentation_changed(snapshot.sequence).await;
    };
    let request = ActionRequest {
        session_id: session.id(),
        owner: "presenter".into(),
        view_id: "question".into(),
        revision: view.revision,
        action: "submit".into(),
        request_id: "same".into(),
        values: json!({ "reason": "valid" }),
    };
    let caller = session.clone();
    let first = tokio::spawn(async move { caller.presentation_action(request.clone()).await });
    entered.notified().await;
    assert_eq!(
        session.presentation_snapshot().views[0].handled_actions,
        vec!["submit"]
    );
    first.abort();
    let retry = ActionRequest {
        session_id: session.id(),
        owner: "presenter".into(),
        view_id: "question".into(),
        revision: view.revision,
        action: "submit".into(),
        request_id: "same".into(),
        values: json!({ "reason": "valid" }),
    };
    assert_eq!(
        session
            .presentation_action(ActionRequest {
                request_id: "other".into(),
                ..retry.clone()
            })
            .await
            .unwrap_err()
            .code,
        "AlreadyHandled"
    );
    release.notify_one();
    assert_eq!(
        session.presentation_action(retry.clone()).await.unwrap(),
        json!({ "calls": 1 })
    );
    assert_eq!(
        session.presentation_action(retry).await.unwrap(),
        json!({ "calls": 1 })
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    session.detach_presentation(attachment).unwrap();
    session.cancel(run).unwrap();
    session.wait(run).await.unwrap();
    session.shutdown().await.unwrap();
}
