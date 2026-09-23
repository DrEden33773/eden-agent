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
    session_with_packages(package, vec![]).await
}

async fn session_with_packages(package: Package, others: Vec<Package>) -> eden_agent::Session {
    let cwd = std::env::current_dir().unwrap();
    let mut embedded = Embedded::new(composition(), cwd.clone())
        .package(
            package
                .service(CONTEXT, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
                .service(PROVIDER, |_: Value, _| async {
                    Ok::<_, Fault>(Value::Null)
                })
                .service(TOOL, |_: Value, _| async { Ok::<_, Fault>(Value::Null) }),
            "presentation-test-v1",
        )
        .unwrap();
    for package in others {
        embedded = embedded.package(package, "second-author-v1").unwrap();
    }
    embedded
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

#[tokio::test]
async fn removing_and_republishing_a_view_never_reuses_an_action_revision() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let action_calls = calls.clone();
    let package =
        Package::new("presenter")
            .service(AGENT_LOOP, |_: RunInput, cx| async move {
                cx.present(View::new("same", Slot::Panel, "Old").node(Node::Button {
                    id: "button".into(),
                    action: "do".into(),
                    label: "Old action".into(),
                }))
                .await?;
                cx.remove_view("same").await?;
                cx.present(View::new("same", Slot::Panel, "New").node(Node::Button {
                    id: "button".into(),
                    action: "do".into(),
                    label: "New action".into(),
                }))
                .await?;
                std::future::pending::<Result<Value, Fault>>().await
            })
            .service(ACTION, move |_: ActionRequest, _| {
                let calls = action_calls.clone();
                async move {
                    Ok::<_, Fault>(json!({ "calls": calls.fetch_add(1, Ordering::SeqCst) + 1 }))
                }
            });
    let session = session(package).await;
    let attachment = session.attach_presentation("web").unwrap();
    let run = session.submit("replace").unwrap();
    let view = loop {
        let snapshot = session.presentation_snapshot();
        if snapshot
            .views
            .first()
            .is_some_and(|view| view.view.title == "New")
        {
            break snapshot.views[0].clone();
        }
        session.presentation_changed(snapshot.sequence).await;
    };
    assert!(view.revision > 1);
    let request = ActionRequest {
        session_id: session.id(),
        owner: "presenter".into(),
        view_id: "same".into(),
        revision: 1,
        action: "do".into(),
        request_id: "old".into(),
        values: Value::Null,
    };
    assert_eq!(
        session
            .presentation_action(request.clone())
            .await
            .unwrap_err()
            .code,
        "StaleRevision"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        session
            .presentation_action(ActionRequest {
                revision: view.revision,
                request_id: "new".into(),
                ..request
            })
            .await
            .unwrap(),
        json!({ "calls": 1 })
    );
    session.detach_presentation(attachment).unwrap();
    session.cancel(run).unwrap();
    session.wait(run).await.unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn run_settlement_waits_for_admitted_action_cleanup() {
    use std::sync::Arc;
    use tokio::sync::Notify;
    for (command, stop) in [
        (false, "cancel"),
        (false, "normal"),
        (false, "shutdown"),
        (true, "cancel"),
        (true, "normal"),
        (true, "shutdown"),
    ] {
        let entered = Arc::new(Notify::new());
        let cleanup_started = Arc::new(Notify::new());
        let release_cleanup = Arc::new(Notify::new());
        let release_run = Arc::new(Notify::new());
        let run_handler = {
            let release = release_run.clone();
            move |_: Value, cx: eden_plugin_sdk::CallContext| {
                let release = release.clone();
                async move {
                    cx.present(View::new("v", Slot::Panel, "V").node(Node::Button {
                        id: "b".into(),
                        action: "do".into(),
                        label: "Do".into(),
                    }))
                    .await?;
                    release.notified().await;
                    Ok::<_, Fault>(Value::Null)
                }
            }
        };
        let package = Package::new("presenter")
            .service(AGENT_LOOP, run_handler.clone())
            .service(eden_protocol::resources::COMMAND, run_handler)
            .service(ACTION, {
                let entered = entered.clone();
                let started = cleanup_started.clone();
                let release = release_cleanup.clone();
                move |_: ActionRequest, cx| {
                    let entered = entered.clone();
                    let started = started.clone();
                    let release = release.clone();
                    async move {
                        cx.scope.cleanup(async move {
                            started.notify_one();
                            release.notified().await;
                            Ok(())
                        })?;
                        entered.notify_one();
                        std::future::pending::<Result<Value, Fault>>().await
                    }
                }
            });
        let session = session(package).await;
        session.attach_presentation("web").unwrap();
        let run = if command {
            session.command("go".into(), Value::Null).unwrap()
        } else {
            session.submit("go").unwrap()
        };
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
            view_id: "v".into(),
            revision: view.revision,
            action: "do".into(),
            request_id: "action1".into(),
            values: Value::Null,
        };
        let caller = session.clone();
        let action_request = request.clone();
        let action = tokio::spawn(async move { caller.presentation_action(action_request).await });
        entered.notified().await;
        let shutdown = if stop == "shutdown" {
            let session = session.clone();
            Some(tokio::spawn(async move { session.shutdown().await }))
        } else {
            if stop == "normal" {
                release_run.notify_one();
            } else {
                session.cancel(run).unwrap();
            }
            None
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            cleanup_started.notified(),
        )
        .await
        .expect("run termination must cancel outstanding actions");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), session.wait(run))
                .await
                .is_err(),
            "settled before action cleanup completed"
        );
        if command {
            assert!(session.state().command_runs.contains(&run));
        } else {
            assert_eq!(session.state().active_run, Some(run));
        }
        assert!(shutdown.as_ref().is_none_or(|task| !task.is_finished()));
        assert!(!action.is_finished());
        assert!(session.submit("too early").is_err());
        assert_eq!(
            session
                .presentation_action(ActionRequest {
                    request_id: "late".into(),
                    ..request.clone()
                })
                .await
                .unwrap_err()
                .code,
            "Unavailable"
        );
        release_cleanup.notify_one();
        let result = action.await.unwrap();
        session.wait(run).await.unwrap();
        assert_eq!(session.presentation_action(request).await, result);
        if let Some(shutdown) = shutdown {
            shutdown.await.unwrap().unwrap();
        }
        session.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn two_authors_have_ordered_panels_and_exclusive_slot_conflicts() {
    const SECOND: &str = "test.second-presenter.v1";
    let second = Package::new("author-b").service(SECOND, |slot: Slot, cx| async move {
        cx.present(View::new("second", slot, "Second author").node(Node::Text {
            id: "text".into(),
            text: "B".into(),
        }))
        .await
    });
    let ready = std::sync::Arc::new(tokio::sync::Notify::new());
    let signal = ready.clone();
    let first = Package::new("author-a").service(AGENT_LOOP, move |_: RunInput, cx| {
        let signal = signal.clone();
        async move {
            for slot in [
                Slot::Panel,
                Slot::ToolResult,
                Slot::Header,
                Slot::Footer,
                Slot::Overlay,
                Slot::Composer,
            ] {
                cx.present(View::new("first", slot, "First author").node(Node::Text {
                    id: "text".into(),
                    text: "A".into(),
                }))
                .await?;
                let other = cx.call::<_, Revision>(SECOND, &slot).await;
                if matches!(slot, Slot::Panel | Slot::ToolResult) {
                    other?;
                } else {
                    assert_eq!(other.unwrap_err().code, "SlotConflict");
                }
            }
            signal.notify_one();
            std::future::pending::<Result<Value, Fault>>().await
        }
    });
    let session = session_with_packages(first, vec![second]).await;
    session.attach_presentation("tui").unwrap();
    session.attach_presentation("web").unwrap();
    let run = session.submit("both").unwrap();
    ready.notified().await;
    loop {
        let snapshot = session.presentation_snapshot();
        if snapshot
            .views
            .iter()
            .any(|view| view.view.slot == Slot::Composer)
        {
            assert_eq!(
                snapshot
                    .views
                    .iter()
                    .map(|view| view.owner.as_str())
                    .collect::<Vec<_>>(),
                vec!["author-a", "author-b"]
            );
            break;
        }
        session.presentation_changed(snapshot.sequence).await;
    }
    session.cancel(run).unwrap();
    session.wait(run).await.unwrap();
    assert!(
        session
            .presentation_snapshot()
            .views
            .iter()
            .all(|view| !view.active)
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn headless_actions_are_unsupported_and_auth_stays_private_beside_public_views() {
    use eden_protocol::models as m;
    let package = Package::new("presenter")
        .service(AGENT_LOOP, |_: RunInput, cx| async move {
            cx.present(
                View::new("v", Slot::Panel, "Public view").node(Node::Button {
                    id: "b".into(),
                    action: "do".into(),
                    label: "Do".into(),
                }),
            )
            .await?;
            std::future::pending::<Result<Value, Fault>>().await
        })
        .service(m::AUTH, |_: m::AuthRequest, _| async {
            Ok::<_, Fault>(m::AuthReply {
                operation_id: Some("private-operation".into()),
                provider: "fixture".into(),
                status: "awaiting_authorization".into(),
                challenge: None,
                source: None,
                interaction: Some(m::AuthInteraction {
                    url: "https://fixture/private-url".into(),
                    user_code: Some("private-code".into()),
                    manual_input: true,
                    expires_at: 0,
                }),
            })
        });
    let session = session(package).await;
    let run = session.submit("headless").unwrap();
    assert_eq!(
        session
            .wait(run)
            .await
            .unwrap()
            .into_result()
            .unwrap_err()
            .code,
        "Unsupported"
    );
    session.attach_presentation("tui").unwrap();
    session.attach_presentation("web").unwrap();
    let run = session.submit("public").unwrap();
    loop {
        let snapshot = session.presentation_snapshot();
        if snapshot.views.iter().any(|view| view.active) {
            break;
        }
        session.presentation_changed(snapshot.sequence).await;
    }
    let before = session.presentation_snapshot();
    let auth = session.auth_status("private-operation").await.unwrap();
    assert_eq!(
        auth.interaction.unwrap().user_code.as_deref(),
        Some("private-code")
    );
    let after = session.presentation_snapshot();
    assert_eq!(before.views, after.views);
    assert!(!json!(after).to_string().contains("private-"));
    assert!(!json!(session.events()).to_string().contains("private-"));
    session.cancel(run).unwrap();
    session.wait(run).await.unwrap();
    session.shutdown().await.unwrap();
}
