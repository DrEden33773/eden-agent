//! Installed manager consumer: shutdown awaits remote cleanup and invalidates retained handles.
use eden_agent::{Outcome, Session};
use eden_plugin_sdk::Cancellation;
use eden_protocol::{
    Request,
    models::{MODEL_MANAGER, ManagerRequest},
};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let composition = args.get(1).ok_or("composition required")?;
    let mode = args.get(2).ok_or("mode required")?;
    tokio::time::timeout(std::time::Duration::from_secs(40), async {
        let session = Session::open(composition).await?;
        let retained = session.role(MODEL_MANAGER)?;
        assert!(!session.managed_models().await?.models.is_empty());
        let run = if mode == "shutdown" {
            let run = session.manage_models(ManagerRequest::Load {
                model: "sdk-shutdown".into(),
                unload_others: false,
            })?;
            let mut sequence = 0;
            loop {
                let events = session.events_after(sequence).await;
                sequence = events.last().map_or(sequence, |event| event.sequence);
                if events.iter().any(|event| {
                    event.run_id == run
                        && event.kind == "model_management"
                        && event.payload["status"] == "accepted"
                }) {
                    break;
                }
                if let Some(terminal) = session.inspect(run) {
                    panic!("manager settled before shutdown barrier: {terminal:?}");
                }
                tokio::task::yield_now().await;
            }
            session.shutdown().await?;
            let terminal = session.wait(run).await?;
            assert_eq!(terminal.outcome, Outcome::Cancelled);
            assert!(terminal.cleanup_errors.is_empty());
            run
        } else {
            let run = session.manage_models(ManagerRequest::Reconnect)?;
            assert!(matches!(
                session.wait(run).await?.outcome,
                Outcome::Completed(_)
            ));
            session.shutdown().await?;
            run
        };
        let stale = retained
            .call(
                Request {
                    execution: None,
                    session_id: session.id(),
                    run_id: run + 1,
                    contract: MODEL_MANAGER.into(),
                    payload: json!(ManagerRequest::List),
                },
                Cancellation::default(),
            )
            .await;
        assert!(matches!(stale.outcome, Outcome::Failed(error) if error.code == "Unavailable"));
        println!(
            "{}",
            json!({ "mode": mode, "stale_handle": "rejected", "cleanup": "joined" })
        );
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await??;
    Ok(())
}
