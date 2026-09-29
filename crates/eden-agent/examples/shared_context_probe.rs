//! Installed shared-context acceptance through public Session APIs.
use eden_agent::{Outcome, Session, SessionOptions};
use eden_protocol::{
    coding::{Block, Item},
    context_edit::{Apply, Rebuild, Scope, Snapshot},
    session_reference::Selection,
};
use serde_json::json;
use std::{error::Error, path::PathBuf, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
type Result<T> = std::result::Result<T, Box<dyn Error>>;

async fn completed(session: &Session, run: u64) -> Result<()> {
    let terminal = tokio::time::timeout(Duration::from_secs(60), session.wait(run)).await??;
    assert!(
        matches!(terminal.outcome, Outcome::Completed(_)),
        "{terminal:?}"
    );
    assert!(terminal.cleanup_errors.is_empty(), "{terminal:?}");
    Ok(())
}
fn replacement(snapshot: &Snapshot, scope: Scope, marker: &str) -> Apply {
    let mut document = snapshot.effective.clone();
    let entry = document
        .entries
        .iter_mut()
        .find(|entry| {
            serde_json::to_string(&entry.item).is_ok_and(|text| text.contains("ORIGINAL-CONTEXT"))
        })
        .expect("seed context entry");
    entry.item = Item::Message {
        role: "user".into(),
        content: vec![Block::Text {
            text: marker.into(),
        }],
    };
    Apply {
        revision: snapshot.revision.clone(),
        document,
        scope,
        source: "installed-shared-context-probe".into(),
    }
}
async fn mark(address: &str, path: &str) -> Result<()> {
    let mut socket = tokio::net::TcpStream::connect(address).await?;
    socket
        .write_all(format!("GET /{path} HTTP/1.0\r\n\r\n").as_bytes())
        .await?;
    let mut response = Vec::new();
    socket.read_to_end(&mut response).await?;
    assert!(response.starts_with(b"HTTP/1.0 200"));
    Ok(())
}
async fn gated(session: &Session) -> Result<()> {
    let mut sequence = 0;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let events = session.events_after(sequence).await;
            sequence = events.last().map_or(sequence, |event| event.sequence);
            if events
                .iter()
                .any(|event| event.kind == "model_text_delta" && event.payload["delta"] == "gate-1")
            {
                break;
            }
            assert!(
                !events
                    .iter()
                    .any(|event| event.kind == "settled" && event.run_id > 1),
                "settled before gate"
            );
        }
    })
    .await?;
    Ok(())
}
async fn authenticate_fixture(session: &Session, provider: &str) -> Result<()> {
    use eden_protocol::models::AuthRequest;
    let auth = session
        .wait(session.authenticate(AuthRequest::Start {
            provider: provider.into(),
        })?)
        .await?;
    assert!(auth.cleanup_errors.is_empty());
    let Outcome::Completed(reply) = auth.outcome else {
        return Err("fixture authentication did not start".into());
    };
    let operation_id = reply["operation_id"]
        .as_str()
        .ok_or("fixture auth operation missing")?
        .to_owned();
    completed(
        session,
        session.authenticate(AuthRequest::Input {
            operation_id,
            api_key: "context-verifier-key".into(),
        })?,
    )
    .await?;
    Ok(())
}
async fn model_images(
    session: &Session,
    blocks_path: &str,
    address: &str,
) -> Result<serde_json::Value> {
    use eden_protocol::{
        context_edit::{ImageAction, ImageEdit},
        models::ModelSelection,
    };
    use std::collections::BTreeMap;
    authenticate_fixture(session, "fixture").await?;
    completed(
        session,
        session.select_model(ModelSelection {
            provider: "fixture".into(),
            model: "vision".into(),
            thinking: None,
        })?,
    )
    .await?;
    let snapshot = session.inspect_context().await?;
    assert_eq!(snapshot.budget["reserve_tokens"]["tokens"], 1024);
    assert_eq!(
        snapshot.budget["reserve_tokens"]["source"],
        json!({ "model": { "provider": "fixture", "model": "vision" } })
    );
    assert_eq!(snapshot.budget["keep_recent_tokens"]["source"], "global");
    assert_eq!(snapshot.image_limits["max_width"], 2);
    let blocks: Vec<Block> = serde_json::from_slice(&std::fs::read(blocks_path)?)?;
    let original_image = serde_json::to_value(
        blocks
            .iter()
            .find(|block| matches!(block, Block::Image { .. }))
            .ok_or("fixture image missing")?,
    )?;
    completed(session, session.submit_blocks(blocks)?).await?;
    let first_history = session.history().await?;
    let first = first_history
        .iter()
        .find(|record| record.kind == "image_version")
        .ok_or("initial image version missing")?
        .payload
        .clone();
    let entry_id = first["entry_id"]
        .as_str()
        .ok_or("image entry identity missing")?
        .to_owned();
    let block_index = first["block_index"].as_u64().ok_or("block index missing")? as usize;
    assert_eq!(first["image"]["payloads"][0], original_image);
    assert_eq!(
        first["image"]["versions"]
            .as_array()
            .ok_or("versions missing")?
            .len(),
        1
    );
    assert_eq!(first["image"]["versions"][0]["width"], 2);
    assert_ne!(
        first["image"]["versions"][0]["payload"],
        first["image"]["original"]
    );
    for (model, code, action, marker) in [
        (
            "tiny",
            "ImageLimitExceeded",
            ImageAction::ReAdapt,
            "tiny-rejected",
        ),
        (
            "text",
            "UnsupportedImageModel",
            ImageAction::Omit,
            "text-rejected",
        ),
    ] {
        completed(
            session,
            session.select_model(ModelSelection {
                provider: "fixture".into(),
                model: model.into(),
                thinking: None,
            })?,
        )
        .await?;
        let snapshot = session.inspect_context().await?;
        assert_eq!(snapshot.budget["reserve_tokens"]["tokens"], 8192);
        assert_eq!(snapshot.budget["reserve_tokens"]["source"], "global");
        if model == "tiny" {
            assert_eq!(snapshot.image_limits["max_width"], 1);
        }
        let before = session.history().await?;
        let terminal = session
            .wait(session.submit(format!("request after switching to {model}"))?)
            .await?;
        assert!(
            matches!(&terminal.outcome, Outcome::Failed(error) if error.code == code),
            "{terminal:?}"
        );
        assert!(terminal.cleanup_errors.is_empty());
        let after = session.history().await?;
        assert_eq!(
            serde_json::to_value(&after[..before.len()])?,
            serde_json::to_value(&before)?
        );
        assert!(
            !after[before.len()..]
                .iter()
                .any(|record| matches!(record.kind.as_str(), "image_version" | "model_request"))
        );
        mark(address, marker).await?;
        let snapshot = session.inspect_context().await?;
        session
            .edit_images(ImageEdit {
                revision: snapshot.revision,
                choices: BTreeMap::from([(
                    entry_id.clone(),
                    BTreeMap::from([(block_index, action)]),
                )]),
            })
            .await?;
        let history = session.history().await?;
        let current = &history
            .iter()
            .rev()
            .find(|record| record.kind == "image_version")
            .ok_or("explicit image version missing")?
            .payload["image"];
        assert_eq!(current["payloads"][0], original_image);
        assert_eq!(current["versions"][0], first["image"]["versions"][0]);
        assert_eq!(
            current["versions"]
                .as_array()
                .ok_or("versions missing")?
                .len(),
            2
        );
        if model == "tiny" {
            assert_eq!(current["versions"][1]["width"], 1);
            assert_eq!(current["versions"][1]["model"], "tiny");
        } else {
            assert!(current["active"].is_null());
            assert_eq!(current["omitted"], true);
        }
        completed(
            session,
            session.submit(format!("explicit image recovery for {model}"))?,
        )
        .await?;
    }
    let final_history = session.history().await?;
    assert_eq!(
        serde_json::to_value(&final_history[..first_history.len()])?,
        serde_json::to_value(&first_history)?
    );
    Ok(json!({
        "initial_width": 2,
        "readapted_width": 1,
        "versions_retained": 2,
        "original_preserved": true,
        "vision_reserve_tokens": 1024,
        "other_reserve_tokens": 8192,
    }))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let session = Session::open_with(
        &args[1],
        SessionOptions {
            cwd: PathBuf::from(&args[2]),
            history: Some(PathBuf::from(&args[3])),
        },
    )
    .await?;
    let requested_mode = args[4].as_str();
    if requested_mode == "notes" {
        authenticate_fixture(&session, "openai").await?;
        completed(
            &session,
            session.select_model(eden_protocol::models::ModelSelection {
                provider: "openai".into(),
                model: "controlled-model".into(),
                thinking: None,
            })?,
        )
        .await?;
    }
    let mode = if requested_mode == "notes" {
        "compact-rebuild"
    } else {
        requested_mode
    };
    let mut report = json!({ "mode": mode });
    if mode == "model-images" {
        report["images"] = model_images(&session, &args[6], &args[5]).await?;
    } else if mode.starts_with("policy-") || mode.starts_with("large-") {
        let before = session.inspect_context().await?;
        if mode == "policy-reopen-branch" {
            assert!(serde_json::to_string(&before.effective)?.contains("POLICY-FIRST-SECOND"));
        } else if mode == "policy-reopen-next_request" {
            assert!(!serde_json::to_string(&before.effective)?.contains("POLICY-FIRST"));
        }
        if mode == "policy-run-finish" {
            session
                .enqueue(
                    "follow_up",
                    vec![Block::Text {
                        text: "FOLLOW-UP-BEFORE-FINISH".into(),
                    }],
                )
                .await?;
        }
        let terminal = session
            .wait(session.submit("policy controlled request")?)
            .await?;
        if matches!(mode, "policy-fail" | "policy-invalid") {
            assert!(
                matches!(&terminal.outcome, Outcome::Failed(error) if error.code == "ContextPolicyFailure"),
                "{terminal:?}"
            );
        } else {
            assert!(
                matches!(terminal.outcome, Outcome::Completed(_)),
                "{terminal:?}"
            );
        }
        assert!(terminal.cleanup_errors.is_empty());
        report["terminal"] = json!(terminal);
        report["effective"] = json!(session.inspect_context().await?.effective);
        if mode == "policy-run-finish" {
            let records = session.history().await?;
            let edits: Vec<_> = records
                .iter()
                .filter(|record| {
                    record.kind == "context_edit" && record.payload["source"] == "policy:first"
                })
                .collect();
            assert_eq!(
                edits.len(),
                1,
                "run_finish must run once after follow-up drainage"
            );
            let requests: Vec<_> = records
                .iter()
                .filter(|record| record.kind == "model_request")
                .collect();
            assert_eq!(requests.len(), 2);
            assert!(
                records
                    .iter()
                    .filter(|record| matches!(
                        record.kind.as_str(),
                        "model_request" | "model_response"
                    ))
                    .all(|record| record.sequence < edits[0].sequence)
            );
            assert!(session.queued().await?.is_empty());
            report["finish_edit_sequence"] = json!(edits[0].sequence);
        }
    } else if mode == "reopen" {
        let snapshot = session.inspect_context().await?;
        assert!(serde_json::to_string(&snapshot.effective)?.contains("EDITED-CONTEXT"));
        assert!(serde_json::to_string(&snapshot.original)?.contains("ORIGINAL-CONTEXT"));
        completed(&session, session.submit("after reopen")?).await?;
    } else if mode.starts_with("reference-") {
        let preview = session.reference_preview(&args[6], None).await?;
        let mut reference = preview.freeze("fixed-reference".into(), &Selection::default())?;
        assert!(serde_json::to_string(&reference.content)?.contains("EDITED-CONTEXT"));
        if mode == "reference-unknown" {
            reference.source_system = None;
        }
        if mode == "reference-different" {
            let snapshot = session.inspect_context().await?;
            let mut document = snapshot.effective;
            document.entries.push(eden_protocol::context_edit::Entry {
                id: "inserted:probe-system".into(),
                item: Item::Message {
                    role: "system".into(),
                    content: vec![Block::Text {
                        text: "TARGET-ONLY-SYSTEM".into(),
                    }],
                },
                references: vec![],
            });
            session
                .edit_context(Apply {
                    revision: snapshot.revision,
                    document,
                    scope: Scope::Branch,
                    source: "probe-target-system".into(),
                })
                .await?;
        }
        // A frozen reference remains usable after its source file is moved away.
        let hidden = PathBuf::from(&args[6]).with_extension("hidden");
        std::fs::rename(&args[6], &hidden)?;
        let run = session.submit_referenced(
            vec![Block::Text {
                text: "quote frozen source".into(),
            }],
            vec![reference],
        )?;
        let terminal = session.wait(run).await?;
        std::fs::rename(hidden, &args[6])?;
        if mode == "reference-budget" {
            assert!(
                matches!(&terminal.outcome, Outcome::Failed(error) if error.code == "ReferenceBudgetExceeded"),
                "{terminal:?}"
            );
        } else {
            assert!(
                matches!(terminal.outcome, Outcome::Completed(_)),
                "{terminal:?}"
            );
        }
        assert!(terminal.cleanup_errors.is_empty());
        report["terminal"] = json!(terminal);
    } else {
        completed(&session, session.submit("ORIGINAL-CONTEXT")?).await?;
        let original = session.history().await?;
        let snapshot = session.inspect_context().await?;
        if mode == "in-flight" {
            let run = session.submit("gated request")?;
            gated(&session).await?;
            let current = session.inspect_context().await?;
            session
                .edit_context(replacement(&current, Scope::Branch, "EDITED-CONTEXT"))
                .await?;
            mark(&args[5], "release").await?;
            completed(&session, run).await?;
        } else {
            let scope = if mode == "next-request" {
                Scope::NextRequest
            } else {
                Scope::Branch
            };
            let edit = replacement(&snapshot, scope, "EDITED-CONTEXT");
            if mode == "compact-rebuild" {
                let mut invalid = edit.clone();
                invalid
                    .document
                    .entries
                    .retain(|entry| !matches!(entry.item, Item::ToolResult { .. }));
                let error = session
                    .edit_context(invalid)
                    .await
                    .expect_err("partial tool group accepted");
                assert_eq!(error.code, "InvalidContext");
                assert_eq!(
                    serde_json::to_value(session.history().await?)?,
                    serde_json::to_value(&original)?
                );
                report["partial_tool_group"] = json!(error.code);
            }
            session.edit_context(edit.clone()).await?;
            let before_rejection = session.history().await?;
            let error = session
                .edit_context(edit)
                .await
                .expect_err("stale revision accepted");
            assert_eq!(error.code, "ContextConflict");
            assert_eq!(
                serde_json::to_value(session.history().await?)?,
                serde_json::to_value(&before_rejection)?
            );
            report["stale_revision"] = json!(error.code);
            completed(&session, session.submit("edited request")?).await?;
            if mode == "next-request" {
                completed(&session, session.submit("later request")?).await?;
            }
            if mode == "compact-rebuild" {
                completed(&session, session.compact(String::new())?).await?;
                completed(&session, session.submit("after compact")?).await?;
                let snapshot = session.inspect_context().await?;
                completed(
                    &session,
                    session.rebuild_context(Rebuild {
                        revision: snapshot.revision,
                        branch: "rebuilt-context".into(),
                        edit_ids: vec![],
                    })?,
                )
                .await?;
                let rebuilt = session.inspect_context().await?;
                assert_eq!(rebuilt.revision.branch, "rebuilt-context");
                let effective = serde_json::to_string(&rebuilt.effective)?;
                assert!(effective.contains("ORIGINAL-CONTEXT"));
                assert!(
                    !effective.contains("EDITED-SUMMARY"),
                    "rebuild resurrected the old compaction or notes summary"
                );
                completed(&session, session.submit("after rebuild")?).await?;
            }
        }
        let durable = session.history().await?;
        assert_eq!(
            serde_json::to_value(&durable[..original.len()])?,
            serde_json::to_value(&original)?
        );
        report["original_prefix_preserved"] = json!(true);
    }
    report["records"] = json!(session.history().await?.len());
    session.shutdown().await?;
    println!("{report}");
    Ok(())
}
