//! Installed notes acceptance uses only public session and SDK contracts.
use eden_agent::{CopyKind, CopyOptions, Outcome, Session, SessionOptions, WorkspaceOptions};
use eden_plugin_sdk::Cancellation;
use eden_protocol::{Request, auxiliary, coding as c, models::ModelSelection, recall};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
async fn call(session: &Session, contract: &str, payload: Value) -> Result<Value> {
    Ok(session
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
        .into_result()?)
}
async fn completed(session: &Session, run: u64) -> Result<()> {
    let terminal = session.wait(run).await?;
    assert!(
        matches!(terminal.outcome, Outcome::Completed(_)),
        "{terminal:?}"
    );
    assert!(terminal.cleanup_errors.is_empty());
    Ok(())
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
async fn recall_pages(session: &Session, sequence: u64) -> Result<()> {
    let mut request = json!({
        "query": { "operation": "read", "sequence": sequence },
        "max_bytes": 17,
        "max_records": 1,
    });
    let mut text = String::new();
    for _ in 0..1000 {
        let page = call(session, "test.recall-author.v1", request.clone()).await?;
        for chunk in page["chunks"].as_array().ok_or("missing recall chunks")? {
            assert_eq!(chunk["sequence"], sequence);
            assert_eq!(chunk["byte_start"], text.len());
            let part = chunk["text"].as_str().ok_or("missing recall text")?;
            assert!(part.len() <= 17);
            text.push_str(part);
        }
        if page["next_cursor"].is_null() {
            assert!(text.contains("rare-紫色-731"));
            return Ok(());
        }
        request["cursor"] = page["next_cursor"].clone();
    }
    Err("recall pagination did not terminate".into())
}
async fn release_notes(scratch: &Path) -> Result<()> {
    let target: Value = serde_json::from_slice(&std::fs::read(scratch.join("target.json"))?)?;
    let base = target["base_url"].as_str().ok_or("missing fixture base")?;
    let address = base
        .strip_prefix("http://")
        .ok_or("fixture is not HTTP")?
        .trim_end_matches("/v1");
    let mut socket = tokio::net::TcpStream::connect(address).await?;
    socket.write_all(b"GET /release HTTP/1.0\r\n\r\n").await?;
    let mut response = Vec::new();
    socket.read_to_end(&mut response).await?;
    assert!(response.starts_with(b"HTTP/1.0 200"));
    Ok(())
}
async fn migration_copies(composition: &Path, scratch: &Path) -> Result<()> {
    let source = scratch.join("history.jsonl");
    let original = std::fs::read(&source)?;
    let mut records = eden_protocol::history::scan_records(&original).records;
    for record in records.iter_mut().filter(|r| r.kind == "extension_state") {
        record.payload["version"] = json!(2);
        record.payload["value"] = json!({ "opaque_v2": "discarded with explicit loss" });
    }
    let future = scratch.join("future-v2.jsonl");
    std::fs::write(
        &future,
        eden_protocol::history::encode_transaction(&records)?,
    )?;
    let future_bytes = std::fs::read(&future)?;
    for (name, public_only) in [("migrated-notes", false), ("public-summary", true)] {
        let destination = scratch.join(format!("{name}.jsonl"));
        let default_composition = scratch.join("default.json");
        let selected = if public_only {
            default_composition.as_path()
        } else {
            composition
        };
        let plan = Session::plan_copy(
            selected,
            CopyOptions {
                source: future.clone(),
                destination: destination.clone(),
                kind: CopyKind::Migrate,
                target: None,
                cwd: None,
                public_only,
            },
        )
        .await?;
        assert!(plan.losses.iter().any(|loss| if public_only {
            loss.contains("private state")
        } else {
            loss.contains("v2")
        }));
        let new_session = plan.new_session;
        Session::apply_copy(plan).await?;
        let scan = eden_protocol::history::scan_records(&std::fs::read(&destination)?);
        assert!(scan.diagnostic.is_none());
        assert_eq!(scan.records[0].session_id, new_session);
        if public_only {
            assert!(!scan.records.iter().any(|r| r.kind == "extension_state"));
        } else {
            let states: Vec<_> = scan
                .records
                .iter()
                .filter(|r| r.kind == "extension_state")
                .collect();
            assert!(!states.is_empty());
            assert!(
                states.iter().all(|r| r.payload["version"] == 1
                    && r.payload["value"]["text"] == r.payload["summary"])
            );
        }
        let restored = Session::open_with_workspace(
            selected,
            SessionOptions {
                cwd: scratch.to_owned(),
                history: Some(destination),
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
        let projected = call(
            &restored,
            c::CONTEXT,
            json!({
                "action": "project",
                "records": restored.history().await?,
                "cwd": scratch.to_string_lossy(),
                "items": [],
            }),
        )
        .await?;
        assert!(projected.to_string().contains("Durable notes:"));
        restored.shutdown().await?;
    }
    assert_eq!(std::fs::read(&source)?, original);
    assert_eq!(std::fs::read(&future)?, future_bytes);
    Ok(())
}
// Model a saved split-package binding without shipping legacy native libraries.
// Checkpoint records are copied unchanged; only the fixture's package identity differs.
async fn switch_legacy_binding(composition: &Path, scratch: &Path) -> Result<Value> {
    let history = scratch.join("history.jsonl");
    let mut records = eden_protocol::history::scan_records(&std::fs::read(&history)?).records;
    let merged = "note-style-context-management";
    for record in records.iter_mut().filter(|r| r.kind == "composition_lock") {
        let binding = &mut record.payload;
        binding["cwd"] = json!(scratch.to_string_lossy());
        let packages = binding["packages"]
            .as_object_mut()
            .ok_or("missing packages")?;
        let package = packages.remove(merged).ok_or("missing notes package")?;
        for (name, services) in [
            (
                "notes",
                vec![
                    "eden.compaction-policy.v1",
                    "eden.record-interpreter.v1",
                    "eden.state-migrator.v1",
                    "eden.configuration.v1",
                ],
            ),
            ("recall", vec![recall::RECALL, recall::TOOLS, recall::TOOL]),
        ] {
            let mut legacy = package.clone();
            legacy["descriptor"]["package"] = json!(name);
            legacy["descriptor"]["provides"] = json!(services);
            legacy["sha256"] = json!("0".repeat(64));
            packages.insert(name.into(), legacy);
        }
        for name in binding["roles"]
            .as_object_mut()
            .ok_or("missing roles")?
            .values_mut()
        {
            if name == merged {
                *name = json!("recall");
            }
        }
        for route in binding["runtime"]["scopes"][""]["bindings"]
            .as_object_mut()
            .ok_or("missing bindings")?
            .values_mut()
        {
            if route["tail"] == merged {
                route["tail"] = json!("notes");
            }
        }
        binding["library_locations"] =
            json!(["legacy/plugins/notes/0.1.0", "legacy/plugins/recall/0.1.0"]);
    }
    std::fs::write(
        &history,
        eden_protocol::history::encode_transaction(&records)?,
    )?;
    let original = std::fs::read(&history)?;
    let options = || SessionOptions {
        cwd: scratch.to_owned(),
        history: Some(history.clone()),
    };
    let workspace = || WorkspaceOptions {
        global_dir: scratch.join("global"),
        project_trust: Some(false),
        overrides: json!({
            "offline_startup": true,
            "discover_skills": false,
            "discover_templates": false,
        }),
    };
    let refused = Session::open_with_workspace(composition, options(), workspace()).await;
    let error = refused.err().ok_or("legacy binding silently replaced")?;
    assert!(error.message.contains("explicitly switch"), "{error:?}");
    assert_eq!(std::fs::read(&history)?, original);
    let switched = Session::open_rebound(composition, options(), workspace()).await?;
    let after = switched.history().await?;
    assert_eq!(json!(&after[..records.len()]), json!(records));
    let binding = &after
        .iter()
        .rev()
        .find(|r| r.kind == "composition_lock")
        .ok_or("missing new binding")?
        .payload;
    assert!(binding["packages"].get(merged).is_some());
    assert!(binding["packages"].get("notes").is_none());
    assert!(binding["packages"].get("recall").is_none());
    switched.shutdown().await?;
    Ok(json!({
        "legacy_binding_requires_explicit_switch": true,
        "original_records_preserved": true,
    }))
}
async fn probe(composition: &Path, scratch: &Path, mode: &str) -> Result<Value> {
    if mode == "legacy-switch" {
        return switch_legacy_binding(composition, scratch).await;
    }
    let options = SessionOptions {
        cwd: if matches!(mode, "missing-interpreter" | "incompatible") {
            scratch.parent().ok_or("missing original cwd")?.to_owned()
        } else {
            scratch.to_owned()
        },
        history: Some(scratch.join("history.jsonl")),
    };
    let workspace = WorkspaceOptions {
        global_dir: scratch.join("global"),
        project_trust: Some(false),
        overrides: json!({
            "offline_startup": true,
            "discover_skills": false,
            "discover_templates": false,
        }),
    };
    let session = if mode == "resume" {
        Session::open_with_workspace(composition, options, workspace).await?
    } else {
        Session::open_rebound(composition, options, workspace).await?
    };
    if matches!(mode, "missing-interpreter" | "incompatible") {
        let records = session.history().await?;
        assert!(records.iter().any(|r| {
            r.kind == "extension_state"
                && r.payload["summary"]
                    .as_str()
                    .is_some_and(|text| text.contains("Durable notes:"))
        }));
        let terminal = session
            .wait(session.submit("must refuse before provider")?)
            .await?;
        assert_eq!(json!(terminal)["outcome"]["status"], "failed");
        assert!(
            json!(terminal).to_string().contains("MissingInterpreter"),
            "{terminal:?}"
        );
        assert!(
            !session
                .events()
                .iter()
                .any(|event| event.kind == "model_usage")
        );
        session.shutdown().await?;
        return Ok(json!({
            "mode": mode,
            "public_summary_readable": true,
            "unsafe_resume_refused": true,
        }));
    }
    if mode == "resume" {
        let records = session.history().await?;
        let source = records
            .iter()
            .find(|r| r.kind == "message" && r.payload.to_string().contains("rare-紫色-731"))
            .ok_or("missing original detail")?;
        recall_pages(&session, source.sequence).await?;
        completed(
            &session,
            session.submit("use restored notes and recall the old detail")?,
        )
        .await?;
        let restored = session.history().await?;
        assert!(restored.iter().any(|r| r.kind == "tool_result" && r.payload.to_string().contains("rare-紫色-731")));
        assert!(
            restored.iter().any(|r| r.kind == "message"
                && r.payload.to_string().contains("Recovered original detail"))
        );
        session.shutdown().await?;
        return Ok(json!({ "process_reopen": true, "recall_pages": true }));
    }
    completed(
        &session,
        session.select_model(ModelSelection {
            provider: "openai".into(),
            model: "notes-fixture".into(),
            thinking: None,
        })?,
    )
    .await?;
    completed(
        &session,
        session.submit("Remember rare-紫色-731 and preserve exact original details")?,
    )
    .await?;
    let before = session.history().await?;
    let old = call(&session, auxiliary::PROVIDER, json!({ "op": "latest" })).await?;
    if matches!(mode, "combined" | "head-conflict") {
        barrier(&scratch.join("warm-held")).await?;
    }
    if matches!(mode, "combined" | "head-conflict") {
        let concurrent = session.clone();
        let target: Value = serde_json::from_slice(&std::fs::read(scratch.join("target.json"))?)?;
        let payload = json!({
            "target": target,
            "action": "compact",
            "records": before,
            "cwd": scratch.to_string_lossy(),
            "items": [],
        });
        let prepare = tokio::spawn(async move {
            concurrent
                .role(c::CONTEXT)?
                .call(
                    Request {
                        execution: None,
                        session_id: concurrent.id(),
                        run_id: 0,
                        contract: c::CONTEXT.into(),
                        payload,
                    },
                    Cancellation::default(),
                )
                .await
                .into_result()
        });
        barrier(&scratch.join("notes-held")).await?;
        assert!(
            !scratch.join("warm-eof").exists(),
            "warming ended before checkpoint generation completed"
        );
        let preparing = call(&session, auxiliary::PROVIDER, json!({ "op": "latest" })).await?;
        for key in ["owner", "revision", "token"] {
            assert_eq!(preparing[key], old[key]);
        }
        if mode == "head-conflict" {
            call(
                &session,
                c::STORE,
                json!(c::StoreRequest::Append {
                    run_id: 0,
                    kind: "metadata".into(),
                    payload: json!({ "name": "concurrent-head" })
                }),
            )
            .await?;
            let changed = session.history().await?;
            release_notes(scratch).await?;
            let error = prepare.await?.expect_err("stale checkpoint committed");
            assert_eq!(error.code, "CheckpointConflict");
            assert_eq!(json!(session.history().await?), json!(changed));
            assert!(
                !changed
                    .iter()
                    .any(|r| matches!(r.kind.as_str(), "extension_state" | "compaction"))
            );
            let latest = call(&session, auxiliary::PROVIDER, json!({ "op": "latest" })).await?;
            assert!(!old.is_null());
            for key in ["owner", "revision", "token"] {
                assert_eq!(latest[key], old[key]);
            }
            assert!(
                !scratch.join("warm-eof").exists(),
                "failed checkpoint cancelled active warming"
            );
            session.shutdown().await?;
            return Ok(json!({
                "head_conflict": true,
                "no_partial_checkpoint": true,
                "projection_not_invalidated": true,
                "active_warming_preserved": true,
            }));
        }
        release_notes(scratch).await?;
        prepare.await??;
    }
    let failed = matches!(
        mode,
        "empty" | "tool" | "failure" | "cancel" | "store-failure"
    );
    if matches!(mode, "threshold" | "overflow" | "post_response") {
        let mut target: Value =
            serde_json::from_slice(&std::fs::read(scratch.join("target.json"))?)?;
        if mode == "threshold" {
            target["limits"]["context_window"] = json!(1);
        }
        call(
            &session,
            c::CONTEXT,
            json!({
                "target": target,
                "action": if mode == "threshold" {
                        "project"
                    } else {
                        mode
                    },
                "records": before,
                "cwd": scratch.to_string_lossy(),
                "items": [],
            }),
        )
        .await?;
    } else if mode != "combined" {
        let run = session.compact("Preserve pending work".into())?;
        if mode == "cancel" {
            barrier(&scratch.join("notes-held")).await?;
            session.cancel(run)?;
        }
        let terminal = session.wait(run).await?;
        if failed {
            assert!(
                !matches!(terminal.outcome, Outcome::Completed(_)),
                "{terminal:?}"
            );
            let after = session.history().await?;
            assert_eq!(json!(&after[..before.len()]), json!(before));
            assert!(
                after[before.len()..]
                    .iter()
                    .all(|record| record.kind == "terminal"),
                "failed preparation changed projection records"
            );
        } else {
            assert!(
                matches!(terminal.outcome, Outcome::Completed(_)),
                "{terminal:?}"
            );
        }
    }
    if failed {
        session.shutdown().await?;
        return Ok(json!({ "mode": mode, "projection_unchanged": true }));
    }
    let records = session.history().await?;
    let checkpoint = records
        .iter()
        .rev()
        .find(|r| r.kind == "compaction")
        .ok_or("missing checkpoint")?;
    assert_eq!(
        checkpoint.payload["reason"],
        if mode == "combined" || mode == "manual" || mode == "author-store" {
            "manual"
        } else {
            mode
        }
    );
    let state = records
        .iter()
        .rev()
        .find(|r| r.kind == "extension_state")
        .ok_or("missing notes")?;
    assert_eq!(state.sequence + 1, checkpoint.sequence);
    assert!(state.payload["required"].as_bool().unwrap_or_default());
    assert!(
        call(
            &session,
            auxiliary::PROVIDER,
            json!({
                "op": "replay",
                "snapshot": old,
                "max_output_tokens": 1,
                "max_age_ms": 60000,
                "timeout_ms": 1000,
            })
        )
        .await
        .is_err()
    );
    if mode == "combined" {
        barrier(&scratch.join("warm-eof")).await?;
        // The fixture's 60001 auxiliary tokens exceed this model's context budget.
        // A fresh foreground projection must still fit without another notes request.
        completed(
            &session,
            session.submit("continue from notes without another compaction")?,
        )
        .await?;
        assert_eq!(
            session
                .history()
                .await?
                .iter()
                .filter(|r| r.kind == "compaction")
                .count(),
            1
        );
    }
    let events = session.events();
    assert!(
        events
            .iter()
            .filter(|e| e.kind == "model_usage")
            .all(|e| e.payload["raw"]["prompt_tokens"] == 11)
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == "auxiliary_usage" && e.payload["purpose"] == "notes")
    );
    assert!(!scratch.join("must-not-exist.txt").exists());
    let source = before
        .iter()
        .find(|r| r.kind == "message" && r.payload.to_string().contains("rare-紫色-731"))
        .ok_or("original missing")?
        .sequence;
    recall_pages(&session, source).await?;
    let empty = call(
        &session,
        recall::RECALL,
        json!({ "query": { "operation": "search", "literal": "never-present-923892" } }),
    )
    .await?;
    assert_eq!(empty["chunks"], json!([]));
    assert_eq!(empty["truncated"], false);
    let mut future = state.payload.clone();
    future["version"] = json!(2);
    let preview = call(
        &session,
        c::MIGRATOR,
        json!({ "states": [future], "apply": false }),
    )
    .await?;
    let applied = call(
        &session,
        c::MIGRATOR,
        json!({ "states": [future], "apply": true }),
    )
    .await?;
    assert_eq!(preview, applied);
    assert!(
        !preview["losses"]
            .as_array()
            .ok_or("missing losses")?
            .is_empty()
    );
    if mode == "manual" {
        let cursor = call(
            &session,
            recall::RECALL,
            json!({ "query": { "operation": "read", "sequence": source }, "max_bytes": 4 }),
        )
        .await?["next_cursor"]
            .clone();
        let rejected = session.navigate(source, "rejected-summary".into(), true)?;
        assert!(!matches!(
            session.wait(rejected).await?.outcome,
            Outcome::Completed(_)
        ));
        completed(
            &session,
            session.navigate(source, "isolated".into(), false)?,
        )
        .await?;
        assert!(
            call(
                &session,
                recall::RECALL,
                json!({ "query": { "operation": "read", "sequence": checkpoint.sequence } })
            )
            .await
            .is_err()
        );
        assert!(
            call(
                &session,
                recall::RECALL,
                json!({
                    "query": { "operation": "read", "sequence": source },
                    "max_bytes": 4,
                    "cursor": cursor,
                })
            )
            .await
            .is_err()
        );
        completed(
            &session,
            session.navigate(checkpoint.sequence, "main".into(), false)?,
        )
        .await?;
    }
    session.shutdown().await?;
    if mode == "manual" {
        let original = std::fs::read(scratch.join("history.jsonl"))?;
        for (name, kind) in [("fork", CopyKind::Fork), ("clone", CopyKind::Clone)] {
            let destination = scratch.join(format!("{name}.jsonl"));
            let plan = Session::plan_copy(
                composition,
                CopyOptions {
                    source: scratch.join("history.jsonl"),
                    destination: destination.clone(),
                    kind,
                    target: None,
                    cwd: None,
                    public_only: false,
                },
            )
            .await?;
            Session::apply_copy(plan).await?;
            let copied = eden_protocol::history::scan_records(&std::fs::read(destination)?);
            assert!(copied.diagnostic.is_none());
            assert_ne!(copied.records[0].session_id, records[0].session_id);
            for state in copied
                .records
                .iter()
                .filter(|r| r.kind == "extension_state")
            {
                for reference in state.payload["references"]
                    .as_array()
                    .ok_or("references missing")?
                {
                    let id = reference.as_u64().ok_or("invalid reference")?;
                    assert!(copied.records.iter().any(|r| r.sequence == id));
                }
            }
            assert!(copied.records.iter().any(|r| r.kind == "compaction"));
        }
        assert_eq!(std::fs::read(scratch.join("history.jsonl"))?, original);
        migration_copies(composition, scratch).await?;
    }
    Ok(json!({
        "mode": mode,
        "atomic_notes_checkpoint": true,
        "old_snapshot_rejected": true,
        "auxiliary_usage_isolated": true,
        "recall_pages": true,
        "migration_preview_matches": true,
    }))
}
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let result = tokio::time::timeout(
        Duration::from_secs(100),
        probe(Path::new(&args[1]), Path::new(&args[2]), &args[3]),
    )
    .await??;
    println!("{result}");
    Ok(())
}
