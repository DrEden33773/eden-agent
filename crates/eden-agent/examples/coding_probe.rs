//! Installed session acceptance probe; waits on a provider event before acting.
use eden_agent::{Session, SessionOptions};
use eden_protocol::{Outcome, coding::Block};
use std::{io::Write, path::PathBuf};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let options = SessionOptions {
        cwd: PathBuf::from(&args[2]),
        history: Some(PathBuf::from(&args[3])),
    };
    if args[4] == "open-abandon" {
        return tokio::time::timeout(
            std::time::Duration::from_secs(20),
            open_abandon(&args[1], options),
        )
        .await?;
    }
    let session = Session::open_with(&args[1], options.clone()).await?;
    let run = session.submit("probe initial")?;
    let mut sequence = 0;
    loop {
        let events = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            session.events_after(sequence),
        )
        .await?;
        sequence = events.last().map_or(sequence, |e| e.sequence);
        if events.iter().any(|e| e.kind == "model_text_delta") {
            break;
        }
        assert!(
            !events.iter().any(|e| e.kind == "settled"),
            "settled before provider gate"
        );
    }
    assert!(
        session.inspect(run).is_none(),
        "acceptance is not completion"
    );
    assert!(session.submit("second active run").is_err());
    let second = Session::open_with(&args[1], options.clone()).await;
    assert!(second.is_err(), "durable session admitted a second writer");
    let mut accepted = vec![];
    for (kind, text) in [
        ("steering", "steer-one"),
        ("steering", "steer-two"),
        ("follow_up", "follow-one"),
        ("follow_up", "follow-two"),
    ] {
        if args[4] == "steering_only" && kind == "follow_up" {
            continue;
        }
        accepted.push(
            session
                .enqueue(kind, vec![Block::Text { text: text.into() }])
                .await?
                .id,
        );
    }
    assert_eq!(
        session
            .queued()
            .await?
            .iter()
            .map(|e| e.id)
            .collect::<Vec<_>>(),
        accepted
    );
    if args[4] == "cancel" {
        session.cancel(run)?;
    } else {
        let mut stream = std::net::TcpStream::connect(&args[5])?;
        stream.write_all(b"GET /release HTTP/1.0\r\n\r\n")?;
    }
    let terminal =
        tokio::time::timeout(std::time::Duration::from_secs(20), session.wait(run)).await??;
    assert!(terminal.cleanup_errors.is_empty());
    let records = session.history().await?;
    if args[4] == "cancel" {
        assert!(!matches!(terminal.outcome, Outcome::Completed { .. }));
        assert_eq!(session.queued().await?.len(), 4);
    } else {
        assert!(matches!(terminal.outcome, Outcome::Completed { .. }));
        assert!(session.queued().await?.is_empty());
        let delivered: Vec<_> = records
            .iter()
            .filter(|r| r.kind == "queue_delivered")
            .map(|r| r.payload["id"].as_u64().unwrap())
            .collect();
        assert_eq!(delivered.len(), accepted.len());
        for pair in [(&accepted[0..2], "steering"), (&accepted[2..], "follow_up")] {
            assert_eq!(
                records
                    .iter()
                    .filter(|r| r.kind == "queue_delivered" && r.payload["kind"] == pair.1)
                    .map(|r| r.payload["id"].as_u64().unwrap())
                    .collect::<Vec<_>>(),
                pair.0
            );
        }
    }
    session.shutdown().await?;
    session.shutdown().await?;
    let reopened = Session::open_with(&args[1], options).await?;
    assert_eq!(reopened.history().await?.len(), records.len());
    assert!(!reopened.events().iter().any(|e| e.kind == "accepted"));
    assert_eq!(
        reopened.queued().await?.len(),
        if args[4] == "cancel" { 4 } else { 0 }
    );
    reopened.shutdown().await?;
    println!(
        "{}",
        serde_json::json!({"terminal":terminal,"queue_ids":accepted,"record_count":records.len(),"reopened_without_execution":true,"writer_exclusion":true,"double_shutdown":true})
    );
    Ok(())
}

async fn open_abandon(
    composition_path: &str,
    options: SessionOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let original = std::fs::read(composition_path)?;
    let mut composition: serde_json::Value = serde_json::from_slice(&original)?;
    for package in composition["packages"]
        .as_array_mut()
        .ok_or("missing packages")?
    {
        if package["descriptor"]["package"] == "coding-replacements" {
            package["config"] = serde_json::json!({"open_gate":listener.local_addr()?.to_string()});
        }
    }
    std::fs::write(composition_path, serde_json::to_vec(&composition)?)?;
    let path = composition_path.to_owned();
    let opening_options = options.clone();
    let opening = tokio::spawn(async move { Session::open_with(path, opening_options).await });
    let (stream, _) = listener.accept().await?;
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    stream.read_line(&mut line).await?;
    assert_eq!(line, "open-ready\n");
    // Lock acquisition, not a sleep or the mere start of initialization, owns this gate.
    let mut lock_path = std::fs::canonicalize(options.history.as_ref().unwrap())?.into_os_string();
    lock_path.push(".lock");
    let competing = std::fs::File::options()
        .read(true)
        .write(true)
        .open(lock_path)?;
    assert!(competing.try_lock().is_err());
    drop(competing);
    opening.abort();
    assert!(matches!(opening.await, Err(error) if error.is_cancelled()));
    stream.get_mut().write_all(b"G").await?;
    let (closed, _) = listener.accept().await?;
    let mut closed = BufReader::new(closed);
    line.clear();
    closed.read_line(&mut line).await?;
    assert_eq!(line, "store-closed\n");
    // Reopen uses the same persisted role identity without the one-shot test gate.
    std::fs::write(composition_path, original)?;
    let reopened = Session::open_with(composition_path, options).await?;
    assert_eq!(reopened.history().await?.len(), 1);
    reopened.shutdown().await?;
    println!(
        "{}",
        serde_json::json!({"open_lock_observed":true,"caller_aborted":true,"store_close_acknowledged":true,"reopened_after_abandoned_delivery":true})
    );
    Ok(())
}
