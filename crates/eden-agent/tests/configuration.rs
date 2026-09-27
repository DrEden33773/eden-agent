//! Configuration transactions use session ownership rather than a synthetic chat run.
use eden_agent::{SessionOptions, WorkspaceOptions, configuration::*, embedded::Embedded};
use eden_plugin_sdk::Package;
use eden_protocol::{
    AGENT_LOOP, CONTEXT, Composition, Fault, PROVIDER, TOOL, configuration as cfg,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn live_apply_survives_waiter_drop_and_rejects_stale_revision_and_secrets() {
    let value = Arc::new(Mutex::new(json!({ "limit": 1, "token": "private-canary" })));
    let updated = value.clone();
    let package = Package::new("config-author")
        .service(AGENT_LOOP, |_: Value, _| async {
            Ok::<_, Fault>(Value::Null)
        })
        .service(CONTEXT, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
        .service(PROVIDER, |_: Value, _| async {
            Ok::<_, Fault>(Value::Null)
        })
        .service(TOOL, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
        .service(cfg::CONFIGURATION, move |request: cfg::PluginRequest, _| {
            let updated = updated.clone();
            async move {
                match request {
                    cfg::PluginRequest::Describe => Ok(json!(cfg::Description {
                        schema: Some(json!({
                            "type": "object",
                            "properties": {
                                "limit": { "type": "integer", "minimum": 1 },
                                "token": { "type": "string" },
                            },
                        })),
                        live_paths: vec!["/limit".into()],
                        secret_paths: vec!["/token".into()],
                        ..Default::default()
                    })),
                    cfg::PluginRequest::Validate { .. } => {
                        Ok(json!(cfg::Validation { errors: vec![] }))
                    }
                    cfg::PluginRequest::Update { config } => {
                        *updated.lock().unwrap() = config;
                        Ok(json!(cfg::Validation { errors: vec![] }))
                    }
                }
            }
        });
    let cwd = std::env::current_dir().unwrap();
    let composition: Composition = serde_json::from_value(json!({
        "packages": [],
        "roles": {},
        "runtime": {
            "instances": [{
                "id": "config-author",
                "package": "config-author",
                "config": { "limit": 1, "token": "private-canary" },
            }],
        },
    }))
    .unwrap();
    let session = Embedded::new(composition, cwd.clone())
        .package(package, "config-v1")
        .unwrap()
        .open(
            SessionOptions { cwd, history: None },
            WorkspaceOptions {
                global_dir: std::env::temp_dir().join(format!("eden-a2-{}", std::process::id())),
                project_trust: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let before = session.inspect_configuration().await.unwrap();
    assert!(
        !serde_json::to_string(&before)
            .unwrap()
            .contains("private-canary")
    );
    let change = Change {
        instance: "config-author".into(),
        revision: before.revision,
        patch: json!({ "limit": 2 }),
        replacement: None,
        edits: vec![],
    };
    let plan = session.preview_configuration(change.clone()).await.unwrap();
    assert_eq!(plan.application, Application::Live);
    let operation = session
        .apply_configuration(change.clone(), ApplyMode::Wait)
        .await
        .unwrap();
    let receipt = session.wait_configuration(operation).await.unwrap();
    assert_eq!(receipt.status, Status::Applied);
    assert_eq!(value.lock().unwrap()["limit"], 2);
    assert_eq!(value.lock().unwrap()["token"], "private-canary");
    assert_eq!(
        session
            .preview_configuration(change)
            .await
            .unwrap_err()
            .code,
        "Conflict"
    );
    let before = session.inspect_configuration().await.unwrap();
    let invalid = Change {
        instance: "config-author".into(),
        revision: before.revision,
        patch: json!({ "limit": 0 }),
        replacement: None,
        edits: vec![],
    };
    assert!(
        !session
            .validate_configuration(invalid)
            .await
            .unwrap()
            .errors
            .is_empty()
    );
    let secret = Change {
        instance: "config-author".into(),
        revision: before.revision,
        patch: json!({ "token": "replacement" }),
        replacement: None,
        edits: vec![],
    };
    assert!(session.preview_configuration(secret).await.is_err());
    let unchanged_secret = Change {
        instance: "config-author".into(),
        revision: before.revision,
        patch: json!({ "token": "private-canary" }),
        replacement: None,
        edits: vec![],
    };
    assert_eq!(
        session
            .preview_configuration(unchanged_secret)
            .await
            .unwrap_err()
            .code,
        "PrivateInputRequired"
    );
    assert!(
        !serde_json::to_string(&session.events())
            .unwrap()
            .contains("private-canary")
    );
    assert!(session.state().active_run.is_none());
    session.shutdown().await.unwrap();
}

async fn running_session(
    dependent: bool,
) -> (
    eden_agent::Session,
    Arc<tokio::sync::Notify>,
    Arc<tokio::sync::Notify>,
    Arc<Mutex<Value>>,
) {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let signal = started.clone();
    let stop = release.clone();
    let input_started = started.clone();
    let foreground = Package::new("foreground")
        .service(eden_protocol::coding::CODING_CONTROL, move |_: Value, _| {
            let input_started = input_started.clone();
            async move {
                input_started.notify_one();
                std::future::pending::<Result<Value, Fault>>().await
            }
        })
        .service(AGENT_LOOP, move |_: Value, _| {
            let signal = signal.clone();
            let stop = stop.clone();
            async move {
                signal.notify_one();
                stop.notified().await;
                Ok::<_, Fault>(json!("done"))
            }
        })
        .service(CONTEXT, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
        .service(PROVIDER, |_: Value, _| async {
            Ok::<_, Fault>(Value::Null)
        })
        .service(TOOL, |_: Value, _| async { Ok::<_, Fault>(Value::Null) });
    let value = Arc::new(Mutex::new(json!({ "limit": 1 })));
    let updated = value.clone();
    let settings = Package::new("settings").service(
        cfg::CONFIGURATION,
        move |request: cfg::PluginRequest, _| {
            let updated = updated.clone();
            async move {
                match request {
                    cfg::PluginRequest::Describe => Ok(json!(cfg::Description {
                        live_paths: vec!["/limit".into()],
                        ..Default::default()
                    })),
                    cfg::PluginRequest::Validate { .. } => Ok(json!(cfg::Validation::default())),
                    cfg::PluginRequest::Update { config } => {
                        *updated.lock().unwrap() = config;
                        Ok(json!(cfg::Validation::default()))
                    }
                }
            }
        },
    );
    let composition = serde_json::from_value(json!({
        "packages": [],
        "roles": {},
        "runtime": {
            "instances": [
                {
                    "id": "foreground",
                    "package": "foreground",
                    "dependencies": if dependent {
                            vec!["settings"]
                        } else {
                            vec![]
                        },
                },
                { "id": "settings", "package": "settings", "config": { "limit": 1 } }
            ],
        },
    }))
    .unwrap();
    let cwd = std::env::current_dir().unwrap();
    let session = Embedded::new(composition, cwd.clone())
        .package(foreground, "foreground-v1")
        .unwrap()
        .package(settings, "settings-v1")
        .unwrap()
        .open(
            SessionOptions { cwd, history: None },
            WorkspaceOptions {
                global_dir: std::env::temp_dir()
                    .join(format!("eden-a2-run-{}", std::process::id())),
                project_trust: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    (session, started, release, value)
}
#[tokio::test]
async fn unrelated_update_does_not_wait_for_foreground_but_related_update_keeps_old_binding() {
    for dependent in [false, true] {
        let (session, started, release, value) = running_session(dependent).await;
        let run = session.submit("wait").unwrap();
        started.notified().await;
        let change = Change {
            instance: "settings".into(),
            revision: 0,
            patch: json!({ "limit": 2 }),
            replacement: None,
            edits: vec![],
        };
        let preview = session.preview_configuration(change.clone()).await.unwrap();
        assert_eq!(
            preview.waiting_runs,
            if dependent { vec![run] } else { vec![] }
        );
        let operation = session
            .apply_configuration(change, ApplyMode::Wait)
            .await
            .unwrap();
        if dependent {
            assert_eq!(
                session.configuration_operation(operation).unwrap().status,
                Status::Waiting
            );
            assert_eq!(value.lock().unwrap()["limit"], 1);
            release.notify_one();
            session.wait(run).await.unwrap().into_result().unwrap();
        }
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                session.wait_configuration(operation)
            )
            .await
            .unwrap()
            .unwrap()
            .status,
            Status::Applied
        );
        if !dependent {
            assert_eq!(session.state().active_run, Some(run));
            release.notify_one();
            session.wait(run).await.unwrap().into_result().unwrap();
        }
        session.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn explicit_cancel_settles_affected_run_before_update_and_detached_waiter_cannot_cancel_apply()
 {
    let (session, started, _release, value) = running_session(true).await;
    let run = session.submit("wait").unwrap();
    started.notified().await;
    let operation = session
        .apply_configuration(
            Change {
                instance: "settings".into(),
                revision: 0,
                patch: json!({ "limit": 3 }),
                replacement: None,
                edits: vec![],
            },
            ApplyMode::Cancel,
        )
        .await
        .unwrap();
    let observer = session.clone();
    let waiter = tokio::spawn(async move { observer.wait_configuration(operation).await });
    waiter.abort();
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.wait_configuration(operation)
        )
        .await
        .unwrap()
        .unwrap()
        .status,
        Status::Applied
    );
    assert_eq!(
        session.wait(run).await.unwrap().outcome,
        eden_agent::Outcome::Cancelled
    );
    assert_eq!(value.lock().unwrap()["limit"], 3);
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn unrelated_pending_control_input_does_not_block_configuration() {
    let (session, started, _, _) = running_session(false).await;
    let control = session.clone();
    let pending = tokio::spawn(async move {
        control
            .control(eden_protocol::coding::CodingControlRequest::Inspect)
            .await
    });
    started.notified().await;
    let operation = session
        .apply_configuration(
            Change {
                instance: "settings".into(),
                revision: 0,
                patch: json!({ "limit": 5 }),
                replacement: None,
                edits: vec![],
            },
            ApplyMode::Wait,
        )
        .await
        .unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        session.wait_configuration(operation),
    )
    .await;
    session.shutdown().await.unwrap();
    assert!(pending.await.unwrap().is_err());
    assert_eq!(result.unwrap().unwrap().status, Status::Applied);
}

#[tokio::test]
async fn management_forms_repeat_apply_validate_and_inherit_without_a_chat_run() {
    use eden_protocol::{configuration_form as f, presentation as p};
    let package = Package::new("form-author")
        .service(AGENT_LOOP, |_: Value, _| async {
            Ok::<_, Fault>(Value::Null)
        })
        .service(CONTEXT, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
        .service(PROVIDER, |_: Value, _| async {
            Ok::<_, Fault>(Value::Null)
        })
        .service(TOOL, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
        .service(
            cfg::CONFIGURATION,
            |request: cfg::PluginRequest, _| async move {
                Ok::<_, Fault>(match request {
                    cfg::PluginRequest::Describe => json!(cfg::Description {
                        schema: Some(json!({
                            "type": "object",
                            "properties": {
                                "limit": { "type": "integer", "minimum": 1 },
                                "key": { "type": "string" },
                                "nested": {
                                    "type": "object",
                                    "properties": { "n": { "type": "integer" } },
                                },
                            },
                        })),
                        live_paths: vec!["/limit".into(), "/nested".into()],
                        secret_paths: vec!["/key".into()],
                        ..Default::default()
                    }),
                    _ => json!(cfg::Validation::default()),
                })
            },
        );
    let cwd = std::env::current_dir().unwrap();
    let composition: Composition = serde_json::from_value(json!({
        "packages": [],
        "roles": {},
        "runtime": {
            "instances": [{
                "id": "form-author",
                "package": "form-author",
                "config": { "limit": 7, "key": "D1-CANARY", "unknown": true, "nested": { "n": 1 } },
            }],
        },
    }))
    .unwrap();
    let session = Embedded::new(composition, cwd.clone())
        .package(package, "forms-v1")
        .unwrap()
        .open(
            SessionOptions { cwd, history: None },
            WorkspaceOptions {
                global_dir: std::env::temp_dir().join(format!("eden-d1-{}", std::process::id())),
                project_trust: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    session.enable_shared_presentation();
    session.open_configuration("form-author").await.unwrap();
    assert!(session.state().active_run.is_none());
    let original = session.presentation_snapshot();
    assert!(
        !serde_json::to_string(&original)
            .unwrap()
            .contains("D1-CANARY")
    );
    let mut custom = original.views[0].view.clone();
    custom.id = "custom-settings".into();
    custom.title = "Author layout".into();
    if let p::Node::ConfigurationForm { fields, .. } = &mut custom.nodes[0] {
        fields.retain(|field| !field.path.starts_with("/nested/"));
        fields.push(f::Field {
            path: "/nested".into(),
            label: "Whole object".into(),
            description: None,
            control: f::Control::Json,
            value: Some(json!({ "n": 1 })),
            options: vec![],
            item_kind: None,
            source: None,
            writable: true,
            configured: false,
        });
    }
    session.publish_configuration(custom).await.unwrap();
    let mut previous: Option<p::ActionRequest> = None;
    for (n, edit) in [
        f::Edit::Set {
            path: "/nested".into(),
            value: json!({ "n": 2 }),
        },
        f::Edit::Set {
            path: "/limit".into(),
            value: json!(8),
        },
        f::Edit::Set {
            path: "/limit".into(),
            value: json!(9),
        },
        f::Edit::Inherit {
            path: "/limit".into(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        let snapshot = session.presentation_snapshot();
        let view = snapshot
            .views
            .iter()
            .find(|view| view.view.id == "custom-settings")
            .unwrap();
        assert_eq!(view.view.title, "Author layout");
        assert_eq!(view.scope, p::ViewScope::Management);
        let p::Node::ConfigurationForm {
            binding, fields, ..
        } = &view.view.nodes[0]
        else {
            panic!("form")
        };
        assert!(
            fields
                .iter()
                .any(|field| field.path == "/nested" && field.writable && field.value.is_some())
        );
        let request = p::ActionRequest {
            session_id: session.id(),
            owner: view.owner.clone(),
            view_id: view.view.id.clone(),
            revision: view.revision,
            action: "settings:apply".into(),
            request_id: format!("apply-{n}"),
            values: json!(f::Submission {
                binding: binding.clone(),
                edits: vec![edit]
            }),
        };
        let result = session.presentation_action(request.clone()).await.unwrap();
        assert_eq!(result["status"], "applied");
        assert_eq!(
            session.presentation_action(request.clone()).await.unwrap(),
            result
        );
        if let Some(mut stale) = previous {
            stale.request_id = format!("stale-{n}");
            assert!(session.presentation_action(stale).await.is_err());
        }
        previous = Some(request);
    }
    let inspect = session.inspect_configuration().await.unwrap();
    assert_eq!(inspect.instances[0].effective["limit"], 7);
    assert_eq!(inspect.instances[0].effective["unknown"], true);
    assert!(session.state().active_run.is_none());
    session.shutdown().await.unwrap();
}
