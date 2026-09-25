//! Startup checks cannot delay business readiness or outlive shutdown.
use eden_agent::{Session, SessionOptions, WorkspaceOptions, embedded::Embedded};
use eden_plugin_sdk::Package;
use eden_protocol::{AGENT_LOOP, CONTEXT, Composition, Fault, PROVIDER, TOOL};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn open(overrides: Value, endpoint: String, calls: Arc<AtomicUsize>) -> Session {
    let package = Package::new("startup-fixture")
        .service(AGENT_LOOP, |_: Value, _| async {
            Ok::<_, Fault>(json!("ready"))
        })
        .service(CONTEXT, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
        .service(PROVIDER, |_: Value, _| async {
            Ok::<_, Fault>(Value::Null)
        })
        .service(TOOL, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
        .service("eden.update-source.v1", move |request: Value, _| {
            let endpoint = endpoint.clone();
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                if request["operation"] == "discover" {
                    return Ok::<_, Fault>(json!({
                        "result": "discovered",
                        "targets": [{
                            "target": { "kind": "host" },
                            "configured": true,
                            "managed": false,
                            "instructions": "fixture",
                        }],
                    }));
                }
                let mut socket = tokio::net::TcpStream::connect(endpoint).await.unwrap();
                socket.write_all(b"check").await.unwrap();
                let mut bytes = [0; 1];
                let _ = socket.read(&mut bytes).await;
                Ok(json!({
                    "result": "checked",
                    "status": {
                        "target": { "kind": "host" },
                        "configured": true,
                        "managed": false,
                        "instructions": "fixture",
                    },
                }))
            }
        });
    let cwd = std::env::current_dir().unwrap();
    Embedded::new(
        Composition {
            host_environment: None,
            runtime: Default::default(),
            packages: vec![],
            roles: BTreeMap::new(),
            resource_packages: vec![],
        },
        cwd.clone(),
    )
    .package(package, "startup-fixture-v1")
    .unwrap()
    .open(
        SessionOptions { cwd, history: None },
        WorkspaceOptions {
            global_dir: std::env::temp_dir().join(format!("eden-startup-{}", std::process::id())),
            project_trust: Some(false),
            overrides,
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn slow_update_endpoint_does_not_block_business_and_shutdown_closes_it() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let session = tokio::time::timeout(
        Duration::from_secs(2),
        open(
            json!({}),
            listener.local_addr().unwrap().to_string(),
            Arc::new(AtomicUsize::new(0)),
        ),
    )
    .await
    .unwrap();
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .expect("startup never checked update source")
        .unwrap();
    let mut request = [0; 5];
    socket.read_exact(&mut request).await.unwrap();
    let run = session.submit("business").unwrap();
    assert_eq!(
        session.wait(run).await.unwrap().into_result().unwrap(),
        json!("ready")
    );
    tokio::time::timeout(Duration::from_secs(2), session.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), socket.read(&mut request))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(
        session
            .events()
            .iter()
            .any(|e| e.kind == "update_check" && e.payload["status"] == "cancelled")
    );
}

#[tokio::test]
async fn disabled_startup_never_invokes_update_source() {
    for settings in [
        json!({ "offline_startup": true }),
        json!({ "update_check": false }),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let session = open(
            settings,
            listener.local_addr().unwrap().to_string(),
            calls.clone(),
        )
        .await;
        let run = session.submit("ready").unwrap();
        session.wait(run).await.unwrap();
        session.shutdown().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn offline_startup_still_allows_explicit_update_networking() {
    use eden_protocol::updates::{Channel, UpdateRequest, UpdateTarget};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let session = open(
        json!({ "offline_startup": true }),
        listener.local_addr().unwrap().to_string(),
        calls.clone(),
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let run = session
        .update(UpdateRequest::Check {
            target: UpdateTarget::Host,
            channel: Channel::Stable,
        })
        .unwrap();
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut request = [0; 5];
    socket.read_exact(&mut request).await.unwrap();
    socket.write_all(b"x").await.unwrap();
    assert_eq!(
        session.wait(run).await.unwrap().into_result().unwrap()["result"],
        "checked"
    );
    session.shutdown().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
