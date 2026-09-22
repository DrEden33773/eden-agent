//! Installed SDK probe for extension state and exact-source copy previews.
use eden_agent::{CopyKind, CopyOptions, Outcome, Session, SessionOptions};
use eden_protocol::coding::ExtensionState;
use serde_json::json;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let composition = PathBuf::from(&args[1]);
    let options = SessionOptions {
        cwd: PathBuf::from(&args[2]),
        history: Some(PathBuf::from(&args[3])),
    };
    if matches!(args[4].as_str(), "summary-rollback" | "summary-cancel") {
        let session = Session::open_with(&composition, options.clone()).await?;
        let initial = session.history().await?;
        let (target, branch) = eden_protocol::history::branch_state(&initial)?;
        let run = session.set_metadata("before-failed-summary".into(), vec![])?;
        session.wait(run).await?.into_result()?;
        let before = session.history().await?;
        let metadata = before
            .iter()
            .find(|r| r.kind == "session_metadata")
            .unwrap()
            .sequence;
        let run = session.navigate(target.unwrap(), "failed-summary".into(), true)?;
        if args[4] == "summary-cancel" {
            // Cancel only after the controlled provider's first delta, so this
            // exercises an in-flight summary rather than pre-admission cancel.
            tokio::time::timeout(std::time::Duration::from_secs(15), async {
                let mut sequence = 0;
                loop {
                    for event in session.events_after(sequence).await {
                        sequence = event.sequence;
                        if event.kind == "model_text_delta" {
                            return;
                        }
                        assert!(
                            event.kind != "settled" || event.run_id != run,
                            "summary settled before its provider delta"
                        );
                    }
                }
            })
            .await?;
            session.cancel(run)?;
        }
        let terminal = session.wait(run).await?;
        assert!(
            if args[4] == "summary-cancel" {
                matches!(terminal.outcome, Outcome::Cancelled)
            } else {
                matches!(terminal.outcome, Outcome::Failed(_))
            },
            "{terminal:?}"
        );
        assert!(terminal.cleanup_errors.is_empty());
        session.shutdown().await?;
        let reopened = Session::open_with(&composition, options).await?;
        let records = reopened.history().await?;
        assert_eq!(eden_protocol::history::branch_state(&records)?.1, branch);
        assert!(
            eden_protocol::history::active_path(&records)?
                .iter()
                .any(|r| r.sequence == metadata)
        );
        reopened.shutdown().await?;
        println!("{}", json!({ "summary_failure_restores_selection": true }));
        return Ok(());
    }
    if args[4] == "stale-copy" {
        let destination = PathBuf::from(&args[5]);
        let plan = Session::plan_copy(
            &composition,
            CopyOptions {
                source: options.history.clone().unwrap(),
                destination: destination.clone(),
                kind: CopyKind::Clone,
                target: None,
                cwd: None,
                public_only: false,
            },
        )
        .await?;
        let session = Session::open_with(&composition, options.clone()).await?;
        let run = session.set_metadata("changed after preview".into(), vec![])?;
        assert!(matches!(
            session.wait(run).await?.outcome,
            Outcome::Completed(_)
        ));
        session.shutdown().await?;
        let bytes = std::fs::read(options.history.as_ref().unwrap())?;
        let error = Session::apply_copy(plan).await.unwrap_err();
        assert!(error.message.contains("source changed"));
        assert!(!destination.exists());
        assert_eq!(std::fs::read(options.history.unwrap())?, bytes);
        println!(
            "{}",
            json!({
                "stale_preview_rejected": true,
                "source_preserved": true,
                "destination_absent": true,
            })
        );
        return Ok(());
    }
    let session = Session::open_with(&composition, options.clone()).await?;
    let version = args.get(5).map_or(Ok(1), |value| value.parse::<u32>())?;
    let namespace = args
        .get(6)
        .cloned()
        .unwrap_or_else(|| "author.counter".into());
    let value = if version == 0 {
        json!({ "value": 17, "label": "preserved" })
    } else {
        json!({ "count": 17, "label": "preserved" })
    };
    let run = session
        .record_state(ExtensionState {
            namespace,
            version,
            required: true,
            summary: "counter needed to continue".into(),
            value,
        })
        .unwrap();
    let terminal = session.wait(run).await?;
    assert!(terminal.cleanup_errors.is_empty());
    assert!(matches!(terminal.outcome, Outcome::Completed(_)));
    let id = session.id();
    let records = session.history().await?;
    session.shutdown().await?;
    // A repeated shutdown stays a success, and reopening the store sees exactly
    // the records the closed session reported.
    session.shutdown().await?;
    let reopened = Session::open_with(&composition, options).await?;
    assert_eq!(reopened.history().await?.len(), records.len());
    reopened.shutdown().await?;
    println!(
        "{}",
        json!({
            "session_id": id,
            "state_committed": true,
            "records": records.len(),
            "double_shutdown": true,
            "version": version,
        })
    );
    Ok(())
}
