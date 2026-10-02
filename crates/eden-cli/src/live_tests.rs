//! T01/T03/T04/T30: real Session admission and transport ownership regressions.
use super::*;
use eden_agent::{SessionOptions, WorkspaceOptions, embedded::Embedded};
use eden_plugin_sdk::Package;
use eden_protocol::{AGENT_LOOP, CONTEXT, Composition, PROVIDER, RunInput, TOOL};
use eden_tui_client::{HostClient, RequestStatus};
use std::collections::BTreeMap;

pub(super) async fn session() -> Session {
    session_with_preview_barrier(None).await
}
async fn session_with_preview_barrier(
    barrier: Option<(
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
        Arc<std::sync::atomic::AtomicBool>,
    )>,
) -> Session {
    let cwd = std::env::current_dir().unwrap();
    let mut package = Package::new("live-test")
        .service(AGENT_LOOP, |input: RunInput, cx| async move {
            if input.prompt == "wait" {
                cx.scope.cancellation().cancelled().await;
                Err(fault("Cancelled", "test cancelled"))
            } else {
                if input.prompt == "snapshot-context" {
                    cx.call::<_, Value>(CONTEXT, &Value::Null).await?;
                }
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
    if let Some((started, release, cleaned)) = barrier {
        package = package.service(
            eden_protocol::delivery::EXPORTER,
            move |_: eden_protocol::delivery::ExportRequest, cx| {
                let started = started.clone();
                let release = release.clone();
                let cleaned = cleaned.clone();
                async move {
                    cx.scope
                        .cleanup(async move {
                            cleaned.store(true, std::sync::atomic::Ordering::SeqCst);
                            Ok(())
                        })
                        .unwrap();
                    started.notify_one();
                    let cancel = cx.scope.cancellation();
                    tokio::select! {
                        _ = cancel.cancelled() => Err(fault("Cancelled", "export cancelled")),
                        _ = release.notified() => Ok(eden_protocol::delivery::Artifact {
                            media_type: "application/x-ndjson".into(),
                            filename: "preview.jsonl".into(),
                            content: "fixed".into(),
                            warnings: vec![]
                        }),
                    }
                }
            },
        );
    }
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
            pid: std::process::id(),
            instance: "test-instance".into(),
        },
    )
    .unwrap();
    let (stop, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        instance: "test-instance".into(),
        management: Default::default(),
        session: session.clone(),
        token: "test-token".into(),
        web_root: None,
        submissions: Mutex::new((HashMap::new(), VecDeque::new())),
        history: tokio::sync::Mutex::new(None),
        presentation_history: tokio::sync::Mutex::new(None),
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
        cwd: "/fixture".into(),
        instance: "test-instance".into(),
        history_path: None,
        reading: None,
        diagnostic: None,
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
async fn idle_snapshots_do_not_refresh_history_from_their_own_store_reads() {
    let cwd = std::env::current_dir().unwrap();
    let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counted = reads.clone();
    let package = Package::new("idle-store")
        .service(AGENT_LOOP, |_: RunInput, _| async {
            Ok::<_, Fault>(Value::Null)
        })
        .service(CONTEXT, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
        .service(PROVIDER, |_: Value, _| async {
            Ok::<_, Fault>(Value::Null)
        })
        .service(TOOL, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
        .service(
            eden_protocol::coding::STORE,
            move |request: eden_protocol::coding::StoreRequest, _| {
                let counted = counted.clone();
                async move {
                    if matches!(request, eden_protocol::coding::StoreRequest::Read) {
                        counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    Ok::<_, Fault>(eden_protocol::coding::StoreReply {
                        active_head: None,
                        active_branch: "main".into(),
                        session_id: 0,
                        sequence: 0,
                        records: vec![],
                    })
                }
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
    .package(package, "idle-store-v1")
    .unwrap()
    .open(
        SessionOptions { cwd, history: None },
        WorkspaceOptions::default(),
    )
    .await
    .unwrap();
    session.enable_shared_presentation();
    let (stop, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        instance: "test-instance".into(),
        management: Default::default(),
        session: session.clone(),
        token: "unused".into(),
        web_root: None,
        submissions: Mutex::new((HashMap::new(), VecDeque::new())),
        history: tokio::sync::Mutex::new(None),
        presentation_history: tokio::sync::Mutex::new(None),
        stop,
    });
    for _ in 0..4 {
        dispatch(&shared, "GET", "/tui/snapshot", Value::Null)
            .await
            .unwrap();
    }
    assert_eq!(
        reads.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "a snapshot's service_called event must not invalidate its own history cache"
    );
    let snapshot = dispatch(&shared, "GET", "/tui/snapshot", Value::Null)
        .await
        .unwrap();
    let cursor = snapshot["events"].as_array().unwrap().last().unwrap()["sequence"]
        .as_u64()
        .unwrap();
    let sequence = snapshot["presentation"]["sequence"].as_u64().unwrap();
    let route = format!("/tui/snapshot?after={sequence}&events_after={cursor}");
    assert!(
        tokio::time::timeout(
            Duration::from_millis(40),
            dispatch(&shared, "GET", &route, Value::Null)
        )
        .await
        .is_err()
    );
    // Real run transitions still invalidate the cache, including run completion.
    let run = session.submit("changed").unwrap();
    session.wait(run).await.unwrap();
    dispatch(&shared, "GET", "/tui/snapshot", Value::Null)
        .await
        .unwrap();
    assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 2);
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn presentation_snapshot_filters_runtime_calls_and_sends_only_the_valid_history_suffix() {
    use eden_protocol::coding::{Record, StoreView};
    let session = session().await;
    let records: Vec<Record> = (1..=3)
        .map(|sequence| Record {
            schema_version: 2,
            session_id: session.id(),
            sequence,
            parent_id: (sequence > 1).then_some(sequence - 1),
            branch: "main".into(),
            run_id: 0,
            kind: if sequence == 2 {
                "model_request"
            } else {
                "message"
            }
            .into(),
            payload: if sequence == 2 {
                json!({ "input": { "fixture_audit": "x".repeat(1048576) }, "prompt_cache": {} })
            } else {
                json!({
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "text", "text": "visible" }],
                })
            },
        })
        .collect();
    let run = session.submit("snapshot-context").unwrap();
    session.wait(run).await.unwrap().into_result().unwrap();
    let cursor = session.events().last().unwrap().sequence;
    let (stop, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        instance: "test-instance".into(),
        management: Default::default(),
        session: session.clone(),
        token: "unused".into(),
        web_root: None,
        submissions: Mutex::new((HashMap::new(), VecDeque::new())),
        history: tokio::sync::Mutex::new(Some((cursor, Arc::new(records.clone())))),
        presentation_history: tokio::sync::Mutex::new(Some((
            cursor,
            Arc::new(
                eden_protocol::history::view_records(&records, StoreView::Presentation).unwrap(),
            ),
        ))),
        stop,
    });
    let full = dispatch(&shared, "GET", "/tui/snapshot", Value::Null)
        .await
        .unwrap();
    assert!(
        full["history"][1]["payload"]["input"]["fixture_audit"]
            .as_str()
            .unwrap()
            .len()
            == 1048576
    );
    assert!(
        full["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "service_called")
    );
    let projected = dispatch(
        &shared,
        "GET",
        "/tui/snapshot?history_view=presentation",
        Value::Null,
    )
    .await
    .unwrap();
    assert!(projected["history"][1]["payload"]["input"].is_null());
    assert!(
        projected["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["kind"] != "service_called")
    );
    assert_eq!(projected["events_cursor"], cursor);
    let route = format!(
        "/tui/snapshot?incremental=1&events_after={cursor}&history_view=presentation&\
         history_delta=1&history_head=1"
    );
    let delta = dispatch(&shared, "GET", &route, Value::Null).await.unwrap();
    assert_eq!(delta["history_append"], true);
    assert_eq!(delta["history"].as_array().unwrap().len(), 2);
    assert_eq!(delta["history"][0]["parent_id"], 1);
    // A selected sibling branch cannot append to an unrelated prior head.
    let route = route.replace("history_head=1", "history_head=99");
    let replacement = dispatch(&shared, "GET", &route, Value::Null).await.unwrap();
    assert_eq!(replacement["history_append"], false);
    assert_eq!(replacement["history"].as_array().unwrap().len(), 3);
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_poll_waits_and_expired_attachment_can_rejoin_same_session() {
    let session = session().await;
    let (stop, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        instance: "test-instance".into(),
        management: Default::default(),
        session: session.clone(),
        token: "unused".into(),
        web_root: None,
        submissions: Mutex::new((HashMap::new(), VecDeque::new())),
        history: tokio::sync::Mutex::new(None),
        presentation_history: tokio::sync::Mutex::new(None),
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
        instance: "test-instance".into(),
        management: Default::default(),
        session: session.clone(),
        token: "fixture".into(),
        web_root: None,
        submissions: Mutex::new((HashMap::new(), VecDeque::new())),
        history: tokio::sync::Mutex::new(None),
        presentation_history: tokio::sync::Mutex::new(None),
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
        instance: "test-instance".into(),
        management: Default::default(),
        session: session.clone(),
        token: "fixture".into(),
        web_root: None,
        submissions: Mutex::new((HashMap::new(), VecDeque::new())),
        history: tokio::sync::Mutex::new(None),
        presentation_history: tokio::sync::Mutex::new(None),
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

#[test]
fn reading_jsonl_is_read_only_and_retains_valid_prefix_and_unknown_content() {
    let root = std::env::temp_dir().join(format!("eden-reading-{}.jsonl", std::process::id()));
    std::fs::write(
        &root,
        concat!(
            "{\"format\":\"eden-reading-v1\",\"restorable\":false}\n",
            "{\"sequence\":3,\"run_id\":1,\"content\":{\"type\":\"message\",\"role\":\"assistant\"\
             ,\"content\":[{\"type\":\"text\",\"text\":\"READING_MARKER\"}]}}\n",
            "{\"content\":{\"type\":\"future\",\"visible\":\"FUTURE_MARKER\"}}\n",
            "{broken"
        ),
    )
    .unwrap();
    let document = read_document(&root).unwrap();
    assert!(document.history.is_empty());
    assert_eq!(document.reading.as_ref().unwrap().entries.len(), 2);
    assert!(document.reading.as_ref().unwrap().diagnostic.is_some());
    assert!(eden_kernel::history::read(&root).is_err());
    std::fs::remove_file(root).unwrap();
}

#[tokio::test]
async fn detaching_frontend_cancels_pending_preview_and_observes_cleanup() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let cleaned = Arc::new(AtomicBool::new(false));
    let session =
        session_with_preview_barrier(Some((started.clone(), release.clone(), cleaned.clone())))
            .await;
    let (stop, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        instance: "test-instance".into(),
        management: Default::default(),
        session: session.clone(),
        token: "fixture".into(),
        web_root: None,
        submissions: Mutex::new((HashMap::new(), VecDeque::new())),
        history: tokio::sync::Mutex::new(None),
        presentation_history: tokio::sync::Mutex::new(None),
        stop,
    });
    let attachment = dispatch(&shared, "POST", "/attach", json!({ "frontend": "tui" }))
        .await
        .unwrap()["attachment"]
        .clone();
    let preparing = shared.clone();
    let owner = attachment.clone();
    let task = tokio::spawn(async move {
        dispatch(
            &preparing,
            "POST",
            "/delivery/preview",
            json!({ "request_id": "pending-preview", "attachment": owner, "selection": {} }),
        )
        .await
    });
    started.notified().await;
    dispatch(
        &shared,
        "POST",
        "/detach",
        json!({ "attachment": attachment }),
    )
    .await
    .unwrap();
    let cleaned_before_ack = cleaned.load(Ordering::SeqCst);
    release.notify_one();
    let result = task.await.unwrap();
    session.shutdown().await.unwrap();
    assert!(
        cleaned_before_ack,
        "detach acknowledged before export cleanup"
    );
    assert!(result.is_err(), "detached preview retained a result");
}
#[test]
fn unknown_history_version_keeps_the_source_visible() {
    let path =
        std::env::temp_dir().join(format!("eden-future-history-{}.jsonl", std::process::id()));
    std::fs::write(
        &path,
        "{\"schema_version\":3,\"content\":\"UNIQUE_UNKNOWN_CONTENT\"}\n",
    )
    .unwrap();
    let document = read_document(&path).unwrap();
    assert!(document.reading.is_some());
    assert!(
        json!(document.reading)
            .to_string()
            .contains("UNIQUE_UNKNOWN_CONTENT")
    );
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn detached_preview_receipt_does_not_retain_the_artifact() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    release.notify_one();
    let session = session_with_preview_barrier(Some((
        started,
        release,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )))
    .await;
    let (stop, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        instance: "test-instance".into(),
        management: Default::default(),
        session: session.clone(),
        token: "fixture".into(),
        web_root: None,
        submissions: Mutex::new((HashMap::new(), VecDeque::new())),
        history: tokio::sync::Mutex::new(None),
        presentation_history: tokio::sync::Mutex::new(None),
        stop,
    });
    let attachment = dispatch(&shared, "POST", "/attach", json!({ "frontend": "tui" }))
        .await
        .unwrap()["attachment"]
        .clone();
    dispatch(
        &shared,
        "POST",
        "/delivery/preview",
        json!({ "request_id": "finished-preview", "attachment": attachment, "selection": {} }),
    )
    .await
    .unwrap();
    dispatch(
        &shared,
        "POST",
        "/detach",
        json!({ "attachment": attachment }),
    )
    .await
    .unwrap();
    let receipt = dispatch(
        &shared,
        "POST",
        "/request-status",
        json!({ "request_id": "finished-preview" }),
    )
    .await
    .unwrap();
    session.shutdown().await.unwrap();
    assert_eq!(receipt["result"]["Err"]["code"], "Cancelled");
    assert!(!receipt.to_string().contains("application/x-ndjson"));
}

#[tokio::test]
async fn lease_expiry_cleans_preview_before_reattachment_and_exit() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let cleaned = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let session =
        session_with_preview_barrier(Some((started.clone(), release.clone(), cleaned.clone())))
            .await;
    let (stop, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        instance: "test-instance".into(),
        management: Default::default(),
        session: session.clone(),
        token: "fixture".into(),
        web_root: None,
        submissions: Mutex::new((HashMap::new(), VecDeque::new())),
        history: tokio::sync::Mutex::new(None),
        presentation_history: tokio::sync::Mutex::new(None),
        stop,
    });
    let old = dispatch(&shared, "POST", "/attach", json!({ "frontend": "tui" }))
        .await
        .unwrap()["attachment"]
        .clone();
    let preparing = shared.clone();
    let owner = old.clone();
    let mut task = tokio::spawn(async move {
        dispatch(
            &preparing,
            "POST",
            "/delivery/preview",
            json!({ "request_id": "expired-preview", "attachment": owner, "selection": {} }),
        )
        .await
    });
    started.notified().await;
    let sequence = session.presentation_snapshot().sequence;
    tokio::time::timeout(
        Duration::from_secs(15),
        session.presentation_changed(sequence),
    )
    .await
    .unwrap();
    assert!(
        session
            .presentation_heartbeat(old.as_u64().unwrap())
            .is_err()
    );
    let automatic = tokio::time::timeout(Duration::from_secs(2), &mut task).await;
    let automatically_cleaned = automatic.is_ok();
    if !automatically_cleaned {
        release.notify_one();
        let _ = task.await;
    }
    let repeated = dispatch(&shared, "POST", "/detach", json!({ "attachment": old })).await;
    let next = dispatch(&shared, "POST", "/attach", json!({ "frontend": "tui" }))
        .await
        .unwrap()["attachment"]
        .clone();
    dispatch(&shared, "POST", "/detach", json!({ "attachment": next }))
        .await
        .unwrap();
    session.shutdown().await.unwrap();
    assert!(
        automatically_cleaned,
        "expired attachment retained its pending preview"
    );
    assert!(repeated.is_ok(), "expired owner cleanup must be idempotent");
    assert_ne!(old, next);
    assert!(cleaned.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn private_key_admission_waits_for_background_context_inspection() {
    use std::{future::Future, task::Poll};
    let session = session().await;
    let (stop, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        instance: "test-instance".into(),
        session: session.clone(),
        token: "fixture".into(),
        web_root: None,
        submissions: Mutex::new((HashMap::new(), VecDeque::new())),
        history: tokio::sync::Mutex::new(None),
        presentation_history: tokio::sync::Mutex::new(None),
        management: Default::default(),
        stop,
    });
    let inspection = shared.management.inspection.lock().await;
    let mut input = Box::pin(dispatch(
        &shared,
        "POST",
        "/auth/input",
        json!({ "api_key": true, "operation_id": "fixture", "input": "fixture-key" }),
    ));
    std::future::poll_fn(|cx| match input.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("private API-key admission bypassed the active inspection"),
    })
    .await;
    drop(inspection);
    let _ = input.await;
    session.shutdown().await.unwrap();
}
