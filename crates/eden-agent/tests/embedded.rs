//! Public embedding without temporary composition files or dynamic libraries.
use eden_agent::{SessionOptions, WorkspaceOptions, embedded::Embedded};
use eden_plugin_sdk::Package;
use eden_protocol::{
    AGENT_LOOP, CONTEXT, Composition, Fault, PROVIDER, RunInput, TOOL,
    interaction::{HOST, Interaction},
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
fn package() -> Package {
    Package::new("test-host")
        .service(AGENT_LOOP, |_: RunInput, cx| async move {
            cx.call::<_, Value>(
                HOST,
                &Interaction::Request {
                    kind: "confirm".into(),
                    title: "Continue?".into(),
                    options: vec![],
                    initial: String::new(),
                    timeout_ms: Some(2000),
                },
            )
            .await
        })
        .service(CONTEXT, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
        .service(PROVIDER, |_: Value, _| async {
            Ok::<_, Fault>(Value::Null)
        })
        .service(TOOL, |_: Value, _| async { Ok::<_, Fault>(Value::Null) })
}
#[tokio::test]
async fn dialog_reply_and_cancellation_share_real_host_bridge_and_close_observation() {
    let cwd = std::env::current_dir().unwrap();
    let session = Embedded::new(composition(), cwd.clone())
        .package(package(), "test-v1")
        .unwrap()
        .open(
            SessionOptions { cwd, history: None },
            WorkspaceOptions {
                global_dir: std::env::temp_dir()
                    .join(format!("eden-embedded-test-{}", std::process::id())),
                ..WorkspaceOptions::default()
            },
        )
        .await
        .unwrap();
    session.set_interactions(true);
    let run = session.submit("first").unwrap();
    let mut sequence = 0;
    let id = loop {
        let events = session.read_events(sequence).await.unwrap();
        sequence = events.last().unwrap().sequence;
        if let Some(event) = events.iter().find(|e| e.kind == "interaction_requested") {
            break event.payload["interaction_id"].as_u64().unwrap();
        }
    };
    assert!(
        session
            .respond_interaction(id, json!("wrong type"))
            .is_err()
    );
    session.respond_interaction(id, json!(true)).unwrap();
    assert_eq!(
        session.wait(run).await.unwrap().into_result().unwrap(),
        json!(true)
    );
    assert!(session.respond_interaction(id, json!(true)).is_err());
    let run = session.submit("second").unwrap();
    loop {
        let events = session.read_events(sequence).await.unwrap();
        sequence = events.last().unwrap().sequence;
        if events
            .iter()
            .any(|e| e.run_id == run && e.kind == "interaction_requested")
        {
            break;
        }
    }
    session.cancel(run).unwrap();
    assert_eq!(
        session
            .wait(run)
            .await
            .unwrap()
            .into_result()
            .unwrap_err()
            .code,
        "Cancelled"
    );
    session.shutdown().await.unwrap();
    let last = session.events().last().unwrap().sequence;
    assert!(session.read_events(last).await.unwrap().is_empty());
}
