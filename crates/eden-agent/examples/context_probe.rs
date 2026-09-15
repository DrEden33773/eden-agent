//! Installed context/queue lifecycle probe; readiness comes from streamed model events.
use eden_agent::{Outcome, Session, SessionOptions};
use eden_protocol::coding::Block;
use serde_json::json;
use std::{error::Error, path::PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

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
    let mode = args[4].as_str();
    let mut ids = vec![];
    if mode == "resume" {
        ids = session
            .queued()
            .await?
            .iter()
            .map(|entry| entry.id)
            .collect();
        assert!(
            !session
                .events()
                .iter()
                .any(|event| event.kind == "accepted")
        );
        mark(&args[5], "opened").await?;
    } else if mode != "compact_cancel" {
        if mode == "all" {
            let run = session.configure_queue("all".into(), "all".into())?;
            let terminal = session.wait(run).await?;
            assert!(matches!(terminal.outcome, Outcome::Completed(_)));
            assert!(terminal.cleanup_errors.is_empty());
        }
        for (kind, text) in if matches!(mode, "one" | "all") {
            vec![
                ("steering", "steer-one"),
                ("steering", "steer-two"),
                ("follow_up", "follow-one"),
                ("follow_up", "follow-two"),
            ]
        } else {
            vec![("steering", "queued-causal-input")]
        } {
            ids.push(
                session
                    .enqueue(kind, vec![Block::Text { text: text.into() }])
                    .await?
                    .id,
            );
        }
    }
    let run = if mode == "compact_cancel" {
        session.compact(String::new())?
    } else if mode == "resume" {
        session.resume()?
    } else {
        session.submit("context probe")?
    };
    if matches!(mode, "cancel_pre" | "cancel_post" | "compact_cancel") {
        let expected = if mode != "cancel_post" {
            "gate-0"
        } else {
            "gate-1"
        };
        let mut sequence = 0;
        loop {
            let events = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                session.events_after(sequence),
            )
            .await?;
            sequence = events.last().map_or(sequence, |event| event.sequence);
            if events
                .iter()
                .any(|event| event.kind == "model_text_delta" && event.payload["delta"] == expected)
            {
                break;
            }
            assert!(
                !events.iter().any(|event| event.kind == "settled"),
                "settled before causal gate"
            );
        }
        if mode == "cancel_post" {
            let records = session.history().await?;
            assert!(
                records
                    .iter()
                    .any(|r| r.kind == "tool_result" && r.payload["call_id"] == "effect")
            );
            assert!(
                records
                    .iter()
                    .any(|r| r.kind == "queue_consumed" && r.payload["id"] == ids[0])
            );
        }
        session.cancel(run)?;
    }
    let terminal =
        tokio::time::timeout(std::time::Duration::from_secs(60), session.wait(run)).await??;
    assert!(terminal.cleanup_errors.is_empty(), "{terminal:?}");
    if mode.starts_with("cancel_") || mode == "compact_cancel" {
        assert!(matches!(terminal.outcome, Outcome::Cancelled));
    } else {
        assert!(
            matches!(terminal.outcome, Outcome::Completed(_)),
            "{terminal:?}"
        );
    }
    let pending = session.queued().await?;
    if mode == "cancel_pre" {
        assert_eq!(
            pending.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            ids
        );
    } else {
        assert!(pending.is_empty());
    }
    let records = session.history().await?;
    session.shutdown().await?;
    println!(
        "{}",
        json!({"mode":mode,"ids":ids,"pending":pending,"records":records.len(),"terminal":terminal})
    );
    Ok(())
}
