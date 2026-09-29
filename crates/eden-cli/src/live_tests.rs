//! T01/T03/T04/T30: real Session admission and transport ownership regressions.
use super::*;
use eden_agent::{SessionOptions, WorkspaceOptions, embedded::Embedded};
use eden_plugin_sdk::Package;
use eden_protocol::{AGENT_LOOP, CONTEXT, Composition, PROVIDER, RunInput, TOOL};
use eden_tui_client::{HostClient, RequestStatus};
use std::collections::BTreeMap;

async fn session() -> Session {
    let cwd = std::env::current_dir().unwrap();
    let package = Package::new("live-test")
        .service(AGENT_LOOP, |input: RunInput, cx| async move {
            if input.prompt == "wait" {
                cx.scope.cancellation().cancelled().await;
                Err(fault("Cancelled", "test cancelled"))
            } else {
                Ok::<_, Fault>(json!(input.prompt))
            }
        })
        .service(CONTEXT, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
        .service(PROVIDER, |_: Value, _| async {
            Ok::<_, Fault>(Value::Null)
        })
        .service(TOOL, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
        .service(
            eden_protocol::models::MODEL_CATALOG,
            |request: Value, _| async move {
                Ok::<_, Fault>(json!({
                    "providers": ["fixture"],
                    "models": [],
                    "target": null,
                    "source": { "kind": "fixture", "location": "local", "updated_at": null },
                    "status": request["action"],
                }))
            },
        );
    let session = Embedded::new(
        Composition {
            host_environment: None,
            runtime: Default::default(),
            packages: vec![],
            roles: BTreeMap::new(),
            resource_packages: vec![],
        },
        cwd.clone(),
    )
    .package(package, "live-test-v1")
    .unwrap()
    .open(
        SessionOptions { cwd, history: None },
        WorkspaceOptions {
            global_dir: std::env::temp_dir()
                .join(format!("eden-live-client-{}", std::process::id())),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    session.enable_shared_presentation();
    session
}

#[tokio::test]
async fn typed_transport_preserves_receipts_detach_and_real_events() {
    let session = session().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let endpoint = std::env::temp_dir().join(format!(
        "eden-live-client-{}-{}.json",
        std::process::id(),
        address.port()
    ));
    write_endpoint(
        &endpoint,
        &Endpoint {
            address: address.to_string(),
            token: "test-token".into(),
            session_id: session.id(),
        },
    )
    .unwrap();
    let (stop, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        management: Default::default(),
        session: session.clone(),
        token: "test-token".into(),
        web_root: None,
        submissions: Mutex::new((HashMap::new(), VecDeque::new())),
        history: tokio::sync::Mutex::new(None),
        stop,
    });
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let shared = shared.clone();
            tokio::spawn(async move {
                serve(stream, Host::Live(shared)).await.unwrap();
            });
        }
    });
    let client = HostClient::new(&endpoint);
    let attachment = client.attach("tui").await.unwrap();
    let foreign = HostClient::for_session(&endpoint, session.id().wrapping_add(1));
    assert_eq!(
        foreign
            .submit_blocks(
                "foreign",
                vec![eden_protocol::coding::Block::Text {
                    text: "must not execute".into()
                }]
            )
            .await
            .unwrap_err()
            .code,
        "SessionMismatch"
    );
    assert!(matches!(
        client.request_status("foreign").await.unwrap(),
        RequestStatus::Unknown
    ));
    assert_eq!(
        foreign.snapshot().await.unwrap_err().code,
        "SessionMismatch"
    );
    let pinned = HostClient::for_session(&endpoint, session.id());
    assert_eq!(
        pinned.snapshot().await.unwrap().state.session_id,
        session.id()
    );

    let content = vec![eden_protocol::coding::Block::Text {
        text: "hello".into(),
    }];
    let run = client
        .submit_blocks("first", content.clone())
        .await
        .unwrap();
    client.detach(attachment).await.unwrap();
    assert_eq!(
        client.wait(run).await.unwrap().into_result().unwrap(),
        json!("hello")
    );
    assert_eq!(client.submit_blocks("first", content).await.unwrap(), run);
    assert!(matches!(
        client.request_status("first").await.unwrap(),
        RequestStatus::Done { result: Ok(_) }
    ));
    assert!(matches!(
        client.request_status("missing").await.unwrap(),
        RequestStatus::Unknown
    ));
    let error = client
        .command("first", "hello", Value::Null)
        .await
        .unwrap_err();
    assert_eq!(error.code, "DuplicateId");
    let snapshot = client.snapshot().await.unwrap();
    assert_eq!(snapshot.state.session_id, session.id());
    assert!(
        snapshot
            .events
            .iter()
            .any(|event| event.run_id == run && event.kind == "settled")
    );
    let waiting = client
        .submit_blocks(
            "second",
            vec![eden_protocol::coding::Block::Text {
                text: "wait".into(),
            }],
        )
        .await
        .unwrap();
    client.cancel(waiting).await.unwrap();
    assert!(client.wait(waiting).await.unwrap().into_result().is_err());
    session.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_file(endpoint).unwrap();
}

#[tokio::test]
async fn static_snapshot_keeps_committed_records_and_is_read_only() {
    let (stop, _) = watch::channel(false);
    let shared = StaticShared {
        history: vec![],
        snapshot: eden_protocol::presentation::Snapshot {
            version: 1,
            session_id: 9,
            sequence: 0,
            views: vec![],
            activity: vec![],
            pending_interactions: vec![],
        },
        token: "unused".into(),
        web_root: None,
        stop,
    };
    let raw = dispatch_static(&shared, "GET", "/tui/snapshot")
        .await
        .unwrap();
    let snapshot: eden_tui_client::Snapshot = serde_json::from_value(raw).unwrap();
    assert!(snapshot.state.read_only);
    assert_eq!(snapshot.state.session_id, 9);
    assert_eq!(
        dispatch_static(&shared, "POST", "/prompt")
            .await
            .unwrap_err()
            .code,
        "Unsupported"
    );
}

#[test]
fn multimodal_submission_does_not_flatten_attachment_blocks() {
    let blocks = vec![
        eden_protocol::coding::Block::Text {
            text: "inspect".into(),
        },
        eden_protocol::coding::Block::Image {
            media_type: "image/png".into(),
            data: "AA==".into(),
        },
        eden_protocol::coding::Block::File {
            name: "notes.txt".into(),
            media_type: "text/plain".into(),
            data: "aGk=".into(),
        },
    ];
    assert_eq!(content(&json!({ "content": blocks })).unwrap(), blocks);
    assert_eq!(
        content(&json!({ "text": "legacy" })).unwrap(),
        vec![eden_protocol::coding::Block::Text {
            text: "legacy".into()
        }]
    );
}

#[tokio::test]
async fn idle_poll_waits_and_expired_attachment_can_rejoin_same_session() {
    let session = session().await;
    let (stop, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        management: Default::default(),
        session: session.clone(),
        token: "unused".into(),
        web_root: None,
        submissions: Mutex::new((HashMap::new(), VecDeque::new())),
        history: tokio::sync::Mutex::new(None),
        stop,
    });
    let attachment = session.attach_presentation("tui").unwrap();
    let initial = dispatch(&shared, "GET", "/tui/snapshot", Value::Null)
        .await
        .unwrap();
    let sequence = initial["presentation"]["sequence"].as_u64().unwrap();
    let events = session.events().last().map_or(0, |event| event.sequence);
    let route =
        format!("/tui/snapshot?attachment={attachment}&after={sequence}&events_after={events}");
    assert!(
        tokio::time::timeout(
            Duration::from_millis(40),
            dispatch(&shared, "GET", &route, Value::Null)
        )
        .await
        .is_err()
    );
    session.detach_presentation(attachment).unwrap();
    assert_eq!(
        dispatch(&shared, "GET", &route, Value::Null)
            .await
            .unwrap_err()
            .code,
        "AttachmentExpired"
    );
    let replacement = dispatch(&shared, "POST", "/attach", json!({ "frontend": "tui" }))
        .await
        .unwrap();
    assert_ne!(replacement["attachment"], json!(attachment));
    let snapshot = dispatch(&shared, "GET", "/tui/snapshot", Value::Null)
        .await
        .unwrap();
    assert_eq!(snapshot["state"]["session_id"], json!(session.id()));
    assert!(snapshot["state"]["active_run"].is_null());
    session.shutdown().await.unwrap();
}

#[test]
fn default_history_uses_project_session_directory() {
    let root = std::env::temp_dir().join(format!("eden-live-history-path-{}", std::process::id()));
    let path = default_history(&root).unwrap();
    assert_eq!(path.parent().unwrap(), root.join(".eden/sessions"));
    assert_eq!(path.extension().unwrap(), "jsonl");
    assert!(!path.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn session_guard_preserves_javascript_safe_and_legacy_requests() {
    assert!(check_session(&json!({}), u64::MAX).is_ok());
    assert!(check_session(&json!({ "session_id": u64::MAX.to_string() }), u64::MAX).is_ok());
    assert_eq!(
        check_session(&json!({ "session_id": 8 }), 7)
            .unwrap_err()
            .code,
        "SessionMismatch"
    );
}

#[tokio::test]
async fn management_catalog_preserves_receipts_and_busy_admission() {
    let session = session().await;
    let (stop, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        management: Default::default(),
        session: session.clone(),
        token: "fixture".into(),
        web_root: None,
        submissions: Mutex::new((HashMap::new(), VecDeque::new())),
        history: tokio::sync::Mutex::new(None),
        stop,
    });
    let catalog = dispatch(&shared, "POST", "/models/list", json!({}))
        .await
        .unwrap();
    assert_eq!(catalog["providers"], json!(["fixture"]));
    let request = json!({ "request_id": "refresh", "request": { "action": "refresh" } });
    let first = dispatch(&shared, "POST", "/models/catalog", request.clone())
        .await
        .unwrap();
    let run = first["run_id"].as_u64().unwrap();
    assert_eq!(
        session.wait(run).await.unwrap().into_result().unwrap()["status"],
        "refresh"
    );
    assert_eq!(
        dispatch(&shared, "POST", "/models/catalog", request)
            .await
            .unwrap(),
        first
    );
    let waiting = session.submit("wait").unwrap();
    let error = dispatch(
        &shared,
        "POST",
        "/models/catalog",
        json!({ "request_id": "busy", "request": { "action": "refresh" } }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "Unavailable");
    session.cancel(waiting).unwrap();
    session.wait(waiting).await.unwrap();
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn public_auth_route_refuses_secret_bodies_before_receipt_storage() {
    let session = session().await;
    let (stop, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        management: Default::default(),
        session: session.clone(),
        token: "fixture".into(),
        web_root: None,
        submissions: Mutex::new((HashMap::new(), VecDeque::new())),
        history: tokio::sync::Mutex::new(None),
        stop,
    });
    let error = dispatch(
        &shared,
        "POST",
        "/auth/start",
        json!({
            "request_id": "private",
            "request": { "action": "input", "operation_id": "op", "api_key": "DO_NOT_PERSIST" },
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "InvalidInput");
    assert!(shared.submissions.lock().unwrap().0.is_empty());
    assert!(
        !json!(session.events())
            .to_string()
            .contains("DO_NOT_PERSIST")
    );
    session.shutdown().await.unwrap();
}
