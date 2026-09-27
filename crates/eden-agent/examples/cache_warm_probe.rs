//! Installed cache warming through public Session, configuration and service contracts.
use eden_agent::{
    Session, SessionOptions, WorkspaceOptions,
    configuration::{ApplyMode, Change, Inspection, Status},
};
use eden_plugin_sdk::Cancellation;
use eden_protocol::{Request, auxiliary, models::ModelSelection};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const CONTROL: &str = "eden.cache-warmer.v1";
async fn call(
    session: &Session,
    contract: &str,
    payload: Value,
) -> std::result::Result<Value, eden_protocol::Fault> {
    session
        .role(contract)?
        .call(
            Request {
                execution: None,
                session_id: session.id(),
                run_id: 0,
                contract: contract.into(),
                payload,
            },
            Cancellation::default(),
        )
        .await
        .into_result()
}
async fn barrier(path: &Path) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}
async fn completed(session: &Session, run: u64) -> Result<()> {
    let terminal = session.wait(run).await?;
    assert_eq!(
        json!(terminal)["outcome"]["status"],
        "completed",
        "{terminal:?}"
    );
    Ok(())
}
fn generation(inspection: &Inspection, id: &str) -> Option<u64> {
    inspection
        .instances
        .iter()
        .find(|item| item.id == id)
        .expect("installed instance")
        .generation
}
async fn snapshot(session: &Session) -> Result<Value> {
    let value = call(session, auxiliary::PROVIDER, json!({ "op": "latest" })).await?;
    assert!(!value.is_null(), "foreground must produce a snapshot");
    Ok(value)
}
async fn stale(session: &Session, snapshot: Value) -> Result<()> {
    let result = call(
        session,
        auxiliary::PROVIDER,
        json!({
            "op": "replay",
            "snapshot": snapshot,
            "max_output_tokens": 1,
            "max_age_ms": 60000,
            "timeout_ms": 1000,
        }),
    )
    .await;
    assert!(
        result.is_err(),
        "invalidated snapshot unexpectedly replayed"
    );
    Ok(())
}
// Every case starts with a real, outstanding HTTP replay, so invalidation must
// cross the provider cleanup barrier before its public operation completes.
async fn active_invalidation(session: &Session, scratch: &Path, cause: &str) -> Result<Value> {
    completed(session, session.submit("active invalidation foreground")?).await?;
    barrier(&scratch.join("aux-held")).await?;
    let old = snapshot(session).await?;
    let original = session.inspect_configuration().await?;
    let control = session.role(CONTROL)?;
    match cause {
        "model" => {
            completed(
                session,
                session.select_model(ModelSelection {
                    provider: "openai".into(),
                    model: "warm-fixture-2".into(),
                    thinking: None,
                })?,
            )
            .await?
        }
        "resource" => completed(session, session.reload_resources()?).await?,
        "branch" => {
            let target = session
                .history()
                .await?
                .iter()
                .find(|record| record.kind == "message" && record.payload["role"] == "user")
                .map(|record| record.sequence)
                .ok_or("user history missing")?;
            completed(
                session,
                session.navigate(target, "active-branch".into(), false)?,
            )
            .await?;
        }
        "projection" => {
            let result = call(
                session,
                eden_protocol::models::MODEL_MANAGER,
                json!({ "action": "reconnect" }),
            )
            .await?;
            assert_eq!(result["status"], "projection-invalidated");
            assert!(
                session
                    .events()
                    .iter()
                    .any(|event| event.kind == "author_projection_committed")
            );
        }
        "off" => {
            let operation = session
                .apply_configuration(
                    Change {
                        instance: "cache-warmer".into(),
                        revision: original.revision,
                        patch: json!({ "mode": "off" }),
                        replacement: None,
                        edits: vec![],
                    },
                    ApplyMode::Wait,
                )
                .await?;
            assert_eq!(
                session.wait_configuration(operation).await?.status,
                Status::Applied
            );
            let changed = session.inspect_configuration().await?;
            for id in ["local-history", "model-access"] {
                assert_eq!(generation(&original, id), generation(&changed, id));
            }
            assert_ne!(
                generation(&original, "cache-warmer"),
                generation(&changed, "cache-warmer")
            );
            assert!(
                control
                    .call(
                        Request {
                            execution: None,
                            session_id: session.id(),
                            run_id: 0,
                            contract: CONTROL.into(),
                            payload: json!({ "op": "status" })
                        },
                        Cancellation::default()
                    )
                    .await
                    .into_result()
                    .is_err()
            );
        }
        _ => return Err("unknown invalidation case".into()),
    }
    barrier(&scratch.join("aux-eof")).await?;
    if cause != "off" {
        stale(session, old).await?;
    }
    let events = session.events();
    let wrappers = events
        .iter()
        .filter(|event| event.kind == "wrapper_called")
        .count();
    assert_eq!(wrappers, 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "model_usage")
            .count(),
        wrappers
    );
    assert!(!scratch.join("must-not-exist.txt").exists());
    Ok(json!({
        "active_invalidation": cause,
        "network_eof_observed": true,
        "wrapper_calls": wrappers,
    }))
}
async fn probe(composition: &Path, scratch: &Path, mode: &str) -> Result<Value> {
    let session = Session::open_with_workspace(
        composition,
        SessionOptions {
            cwd: scratch.to_owned(),
            history: Some(scratch.join("history.jsonl")),
        },
        WorkspaceOptions {
            global_dir: scratch.join("global"),
            project_trust: Some(false),
            overrides: json!({
                "offline_startup": true,
                "discover_skills": false,
                "discover_templates": false,
            }),
        },
    )
    .await?;
    completed(
        &session,
        session.select_model(ModelSelection {
            provider: "openai".into(),
            model: "warm-fixture".into(),
            thinking: None,
        })?,
    )
    .await?;
    if !matches!(mode, "idle" | "streaming") {
        let result = active_invalidation(&session, scratch, mode).await?;
        session.shutdown().await?;
        return Ok(result);
    }
    let original = session.inspect_configuration().await?;
    let first = session.submit("first foreground")?;
    if mode == "streaming" {
        barrier(&scratch.join("aux-held")).await?;
        completed(&session, first).await?;
        barrier(&scratch.join("aux-eof")).await?;
    } else {
        completed(&session, first).await?;
        barrier(&scratch.join("aux-held")).await?;
        let old = snapshot(&session).await?;
        completed(&session, session.submit("second foreground")?).await?;
        barrier(&scratch.join("aux-eof")).await?;
        stale(&session, old).await?;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let status = call(&session, CONTROL, json!({ "op": "status" })).await?;
                if status["requests"].as_u64().unwrap_or_default() >= 3
                    && session
                        .events()
                        .iter()
                        .filter(|event| event.kind == "auxiliary_usage")
                        .count()
                        >= 2
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, eden_protocol::Fault>(())
        })
        .await??;
    }
    let old_control = session.role(CONTROL)?;
    let operation = session
        .apply_configuration(
            Change {
                instance: "cache-warmer".into(),
                revision: original.revision,
                patch: json!({ "mode": "off" }),
                replacement: None,
                edits: vec![],
            },
            ApplyMode::Wait,
        )
        .await?;
    assert_eq!(
        session.wait_configuration(operation).await?.status,
        Status::Applied
    );
    let changed = session.inspect_configuration().await?;
    assert_ne!(
        generation(&original, "cache-warmer"),
        generation(&changed, "cache-warmer")
    );
    for id in ["local-history", "model-access"] {
        assert_eq!(generation(&original, id), generation(&changed, id));
    }
    assert_eq!(
        call(&session, CONTROL, json!({ "op": "status" })).await?["status"],
        "disabled"
    );
    assert!(
        old_control
            .call(
                Request {
                    execution: None,
                    session_id: session.id(),
                    run_id: 0,
                    contract: CONTROL.into(),
                    payload: json!({ "op": "status" }),
                },
                Cancellation::default()
            )
            .await
            .into_result()
            .is_err(),
        "old warmer handle survived replacement"
    );
    // Disabled warming still permits foreground snapshots; management changes retire them.
    completed(&session, session.submit("model invalidation")?).await?;
    let old = snapshot(&session).await?;
    completed(
        &session,
        session.select_model(ModelSelection {
            provider: "openai".into(),
            model: "warm-fixture-2".into(),
            thinking: None,
        })?,
    )
    .await?;
    stale(&session, old).await?;
    completed(&session, session.submit("resource invalidation")?).await?;
    let old = snapshot(&session).await?;
    completed(&session, session.reload_resources()?).await?;
    stale(&session, old).await?;
    completed(&session, session.submit("branch invalidation")?).await?;
    let old = snapshot(&session).await?;
    let target = session
        .history()
        .await?
        .iter()
        .find(|record| record.kind == "message" && record.payload["role"] == "user")
        .map(|record| record.sequence)
        .ok_or("user history missing")?;
    completed(
        &session,
        session.navigate(target, "probe-branch".into(), false)?,
    )
    .await?;
    stale(&session, old).await?;
    let history = serde_json::to_string(&session.history().await?)?;
    assert!(!history.contains("AUXILIARY-ONLY-CANARY"));
    assert!(!scratch.join("must-not-exist.txt").exists());
    let events = session.events();
    let auxiliary_usage = events
        .iter()
        .filter(|event| event.kind == "auxiliary_usage")
        .count();
    if mode == "idle" {
        assert!(auxiliary_usage >= 2);
        assert!(
            events
                .iter()
                .filter(|event| event.kind == "auxiliary_usage")
                .all(|event| event.payload["usage"]["raw"]["prompt_tokens"] == 901)
        );
    }
    assert!(
        events
            .iter()
            .filter(|event| event.kind == "auxiliary_usage")
            .all(|event| event.payload["purpose"] == "cache_warm")
    );
    assert!(!events.iter().any(|event| event.kind == "model_text_delta"
        && event.payload.to_string().contains("AUXILIARY-ONLY-CANARY")));
    let foreground_usage: Vec<_> = events
        .iter()
        .filter(|event| event.kind == "model_usage")
        .collect();
    assert!(!foreground_usage.is_empty());
    let wrappers = events
        .iter()
        .filter(|event| event.kind == "wrapper_called")
        .count();
    assert_eq!(
        wrappers,
        foreground_usage.len(),
        "auxiliary replay reran the foreground wrapper"
    );
    assert!(
        foreground_usage
            .iter()
            .all(|event| event.payload["raw"]["prompt_tokens"] == 11)
    );
    session.shutdown().await?;
    Ok(json!({
        "mode": mode,
        "wrapper_calls": wrappers,
        "network_eof_observed": true,
        "local_restart_preserves_store_provider": true,
        "stale_model_resource_branch_snapshots_rejected": true,
        "auxiliary_usage_events": auxiliary_usage,
        "output_and_tool_isolated": true,
    }))
}
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        probe(Path::new(&args[1]), Path::new(&args[2]), &args[3]),
    )
    .await??;
    println!("{result}");
    Ok(())
}
