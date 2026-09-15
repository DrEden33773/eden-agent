//! Installed S3 lifecycle probe, fixed before the independent authors are built.
use eden_agent::{Session, SessionOptions, WorkspaceOptions};
use eden_plugin_sdk::Cancellation;
use eden_protocol::{Request, coding as c, resources as r};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
async fn call(
    session: &Session,
    contract: &str,
    payload: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(session
        .role(contract)?
        .call(
            Request {
                session_id: session.id(),
                run_id: 99,
                contract: contract.into(),
                payload,
            },
            Cancellation::default(),
        )
        .await
        .into_result()?)
}
async fn search(
    session: &Session,
    pattern: &str,
    arguments: Value,
) -> Result<(Value, String), Box<dyn std::error::Error>> {
    let mut args = json!({"pattern":pattern});
    args.as_object_mut()
        .unwrap()
        .extend(arguments.as_object().unwrap().clone());
    let result: c::ToolResult = serde_json::from_value(
        call(
            session,
            r::SEARCH,
            json!(c::ToolRequest {
                cwd: session.cwd().into(),
                call_id: "probe-search".into(),
                name: "grep".into(),
                arguments: args
            }),
        )
        .await?,
    )?;
    let header = serde_json::from_str(result.text.lines().next().ok_or("no search header")?)?;
    Ok((header, result.text))
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let composition = PathBuf::from(&args[1]);
    let cwd = std::fs::canonicalize(&args[2])?;
    let mode = &args[3];
    let options = || SessionOptions {
        cwd: cwd.clone(),
        history: Some(cwd.join(format!("{mode}.jsonl"))),
    };
    let workspace = || WorkspaceOptions {
        global_dir: cwd.join("global"),
        project_trust: Some(true),
        overrides: json!({}),
    };
    let session = Session::open_with_workspace(&composition, options(), workspace()).await?;
    let result = match mode.as_str() {
        "switch" => {
            let old = session.role(c::LOOP)?;
            let bad = PathBuf::from(&args[4]);
            let failed_init = PathBuf::from(&args[5]);
            let switched = session.switch_composition(bad)?;
            assert!(session.wait(switched).await?.into_result().is_err());
            assert!(session.role(c::LOOP).is_ok());
            let before = session
                .history()
                .await?
                .into_iter()
                .rev()
                .find(|r| r.kind == "composition_lock")
                .unwrap();
            let switched = session.switch_composition(failed_init)?;
            assert!(session.wait(switched).await?.into_result().is_err());
            assert!(session.role(c::LOOP).is_err());
            assert_eq!(
                session
                    .history()
                    .await?
                    .into_iter()
                    .rev()
                    .find(|r| r.kind == "composition_lock")
                    .unwrap()
                    .payload,
                before.payload
            );
            let switched = session.switch_composition(composition.clone())?;
            session.wait(switched).await?.into_result()?;
            assert_eq!(
                old.call(
                    Request {
                        session_id: session.id(),
                        run_id: 99,
                        contract: c::LOOP.into(),
                        payload: Value::Null
                    },
                    Cancellation::default()
                )
                .await
                .into_result()
                .unwrap_err()
                .code,
                "Unavailable"
            );
            json!({
            "preflight_preserved":true,
            "failed_init_unavailable":true,
            "old_binding_preserved":true,
            "explicit_recovery":true,
            "old_handle_rejected":true
            })
        }
        "interop" => {
            let reply = call(&session, "example.client.v1", json!({"value":6})).await?;
            assert_eq!(reply["result"]["answer"], 42);
            assert_eq!(reply["via"], "independent-b");
            let command = session.command("example.compute".into(), json!({"value":7}))?;
            let answer = session.wait(command).await?.into_result()?;
            assert_eq!(answer["answer"], 49);
            assert!(
                session
                    .resources()
                    .await?
                    .instructions
                    .contains("external-a")
            );
            json!({"unknown_contract_call":reply,"command":answer,"independent_resource_source":true})
        }
        "resources" => {
            let before = session.resources().await?;
            assert!(before.instructions.contains("FIRST-INSTRUCTION"));
            std::fs::write(
                cwd.join("global/skills/check/SKILL.md"),
                "---\nname: [bad\n---\nbroken",
            )?;
            let reload = session.reload_resources()?;
            assert!(session.wait(reload).await?.into_result().is_err());
            assert_eq!(session.resources().await?.revision, before.revision);
            std::fs::write(
                cwd.join("global/skills/check/SKILL.md"),
                "---\nname: check\ndescription: Check marker\n---\nSKILL-BODY",
            )?;
            std::fs::write(cwd.join("AGENTS.md"), "SECOND-INSTRUCTION")?;
            let reload = session.reload_resources()?;
            session.wait(reload).await?.into_result()?;
            assert_eq!(session.resources().await?.revision, before.revision + 1);
            assert!(
                session
                    .resources()
                    .await?
                    .instructions
                    .contains("SECOND-INSTRUCTION")
            );
            json!({"failed_reload_retained_snapshot":true,"explicit_atomic_reload":true})
        }
        "cancel-search" => {
            let root = cwd.join("cancel-corpus");
            std::fs::create_dir(&root)?;
            for i in 0..300 {
                std::fs::write(
                    root.join(format!("{i}.txt")),
                    "needle content\n".repeat(10000),
                )?;
            }
            let role = session.role(r::SEARCH)?;
            let cancel = Cancellation::default();
            let signal = cancel.clone();
            let id = session.id();
            let dir = session.cwd().to_owned();
            let mut sequence = session.events().last().map_or(0, |e| e.sequence);
            let task = tokio::spawn(async move {
                role.call(
                    Request {
                        session_id: id,
                        run_id: 100,
                        contract: r::SEARCH.into(),
                        payload: json!(c::ToolRequest {
                            cwd: dir,
                            call_id: "cancel-search".into(),
                            name: "grep".into(),
                            arguments: json!({"pattern":"needle","path":"cancel-corpus"})
                        }),
                    },
                    signal,
                )
                .await
            });
            let pid = loop {
                let events = session.events_after(sequence).await;
                sequence = events.last().unwrap().sequence;
                if let Some(event) = events.iter().find(|e| e.kind == "search_index_started") {
                    break event.payload["worker_pid"].clone();
                }
            };
            cancel.cancel();
            assert!(matches!(
                task.await?.outcome,
                eden_protocol::Outcome::Cancelled
            ));
            std::fs::remove_dir_all(root)?;
            json!({"cancelled_after_native_index_started":true,"stopped_worker_pid":pid})
        }
        "search" => {
            std::fs::create_dir_all(cwd.join("src"))?;
            std::fs::write(cwd.join("src/matches.txt"), "needle\n".repeat(61))?;
            let (first, _) = search(&session, "needle", json!({"path":"src","limit":17})).await?;
            assert_eq!(first["total_matches"], 61);
            assert_eq!(first["returned"], 17);
            let pid = first["index"]["worker_pid"].clone();
            let mut page = first.clone();
            let mut total = 17;
            while page["has_more"] == true {
                (page, _) = search(
                    &session,
                    "needle",
                    json!({"path":"src","limit":17,"cursor":page["cursor"]}),
                )
                .await?;
                total += page["returned"].as_u64().unwrap();
            }
            assert_eq!(total, 61);
            std::fs::write(cwd.join("src/matches.txt"), "new-content\n")?;
            assert_eq!(
                search(&session, "new-content", json!({"path":"src"}))
                    .await?
                    .0["total_matches"],
                1
            );
            let other = Session::open_with_workspace(
                &composition,
                SessionOptions {
                    cwd: cwd.clone(),
                    history: None,
                },
                workspace(),
            )
            .await?;
            let (independent, _) = search(&other, "new-content", json!({"path":"src"})).await?;
            let other_pid = independent["index"]["worker_pid"].clone();
            assert_ne!(pid, other_pid);
            other.shutdown().await?;
            assert!(
                search(
                    &session,
                    "needle",
                    json!({"path":"src","limit":17,"cursor":first["cursor"]})
                )
                .await
                .is_err()
            );
            json!({
            "same_file_pagination":total,
            "updated_without_refresh":true,
            "independent_worker_pids":[pid,
            other_pid],
            "stale_cursor_rejected":true
            })
        }
        _ => return Err("unknown probe mode".into()),
    };
    session.shutdown().await?;
    if mode == "switch" {
        let reopened = Session::open_with_workspace(&composition, options(), workspace()).await?;
        reopened.shutdown().await?;
    }
    if mode == "interop" {
        assert!(Path::new(&args[4]).is_file());
    }
    println!("{result}");
    Ok(())
}
