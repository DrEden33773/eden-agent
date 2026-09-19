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
