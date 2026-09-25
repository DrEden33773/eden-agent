//! Executed by the installed-artifact verifier, using the public session API.
use eden_agent::{Outcome, Session};
use eden_plugin_sdk::Cancellation;
use eden_protocol::{AGENT_LOOP, Request};
use std::path::Path;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
type Error = Box<dyn std::error::Error>;
async fn connection(listener: &TcpListener, expected: &str) -> Result<BufReader<TcpStream>, Error> {
    let (stream, _) = listener.accept().await?;
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    stream.read_line(&mut line).await?;
    assert_eq!(line.trim(), expected);
    Ok(stream)
}
async fn root_and_child(streams: [TcpStream; 2]) -> Result<[BufReader<TcpStream>; 2], Error> {
    // The greeting identifies ownership; listener arrival order is not a lifecycle barrier.
    let mut root = None;
    let mut child = None;
    for stream in streams {
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream.read_line(&mut line).await?;
        let slot = match line.trim() {
            "root" => &mut root,
            "child" => &mut child,
            identity => return Err(format!("unexpected lifecycle identity: {identity:?}").into()),
        };
        if slot.is_some() {
            return Err(format!("duplicate lifecycle identity: {:?}", line.trim()).into());
        }
        *slot = Some(stream);
    }
    Ok([
        root.ok_or("root connection missing")?,
        child.ok_or("child connection missing")?,
    ])
}
#[tokio::main]
async fn main() -> Result<(), Error> {
    let args: Vec<_> = std::env::args().collect();
    let composition = args.get(1).ok_or("composition required")?;
    let mode = args.get(2).ok_or("mode required")?;
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        probe(Path::new(composition), mode),
    )
    .await??;
    Ok(())
}
async fn probe(path: &Path, mode: &str) -> Result<(), Error> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let mut composition: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    for package in composition["packages"]
        .as_array_mut()
        .ok_or("packages missing")?
    {
        if package["descriptor"]["package"] == "lifecycle" {
            package["config"] = serde_json::json!({
                "address": listener.local_addr()?.to_string(),
                "mode": mode,
            });
        }
    }
    let local_path = path.with_file_name(format!("probe-{mode}.json"));
    std::fs::write(&local_path, serde_json::to_vec(&composition)?)?;
    let session = Session::open(&local_path).await?;
    let role = session.role(AGENT_LOOP)?;
    let run = session.submit("hello")?;
    assert!(session.submit("overlap").is_err());
    let streams = [listener.accept().await?.0, listener.accept().await?.0];
    let [mut root, mut child] = root_and_child(streams).await?;
    let mut sequence = 0;
    loop {
        let events = session.events_after(sequence).await;
        sequence = events.last().map_or(sequence, |e| e.sequence);
        if events
            .iter()
            .any(|e| e.run_id == run && e.kind == "waiting")
        {
            break;
        }
    }
    // Waiting for this run cannot stop another session or its independently owned instances.
    let other_path = path.with_file_name("composition.json");
    let other = Session::open(other_path).await?;
    let other_run = other.submit("independent")?;
    assert!(matches!(
        other.wait(other_run).await?.outcome,
        Outcome::Completed(_)
    ));
    let dropped = session.clone();
    let receiver = tokio::spawn(async move { dropped.wait(run).await });
    if mode == "dropped_receiver" {
        receiver.abort();
    }
    if ["complete", "failed_then_cancel", "panic"].contains(&mode) {
        root.write_all(b"F").await?;
    } else {
        session.cancel(run)?;
    }
    let mut stopping = String::new();
    child.read_line(&mut stopping).await?;
    assert_eq!(stopping.trim(), "child-stopping");
    assert!(
        session.inspect(run).is_none(),
        "settled while child completion gate held"
    );
    assert!(
        !session
            .events()
            .iter()
            .any(|e| e.kind == "settled" && e.run_id == run)
    );
    child.write_all(b"G").await?;
    let mut stopped = String::new();
    child.read_line(&mut stopped).await?;
    assert_eq!(stopped.trim(), "child-stopped");
    let mut byte = [0];
    assert_eq!(
        child.read(&mut byte).await?,
        0,
        "child resource remained live after completion"
    );
    let mut cleanup = connection(&listener, "cleanup").await?;
    assert_eq!(root.read(&mut byte).await?, 0, "root socket was not closed");
    assert!(
        session.inspect(run).is_none(),
        "settled before cleanup gate release"
    );
    assert!(
        !session
            .events()
            .iter()
            .any(|e| e.kind == "settled" && e.run_id == run)
    );
    if mode == "failed_then_cancel" {
        session.cancel(run)?;
    }
    cleanup.write_all(b"G").await?;
    let mut done = String::new();
    cleanup.read_line(&mut done).await?;
    assert_eq!(done.trim(), "cleanup-done");
    assert_eq!(
        cleanup.read(&mut byte).await?,
        0,
        "cleanup socket remained open"
    );
    let terminal = session.wait(run).await?;
    match mode {
        "complete" => assert!(matches!(terminal.outcome, Outcome::Completed(_))),
        "failed_then_cancel" => assert!(matches!(
            &terminal.outcome,
            Outcome::Failed(error)
                if error.code == "ProviderFailure" && error.source == "author-root"
        )),
        "panic" => assert!(
            matches!(&terminal.outcome, Outcome::Failed(error) if error.code == "PluginFailure")
        ),
        _ => assert_eq!(terminal.outcome, Outcome::Cancelled),
    }
    if ["cleanup_error", "drop_panic"].contains(&mode) {
        assert_eq!(terminal.cleanup_errors.len(), 1);
    } else {
        assert!(terminal.cleanup_errors.is_empty());
    }
    if mode != "dropped_receiver" {
        receiver.await??;
    }
    let retained = session.inspect(run).ok_or("terminal not retained")?;
    assert_eq!(retained, terminal);
    session.shutdown().await?;
    assert!(session.submit("after shutdown").is_err());
    let stale = role
        .call(
            Request {
                execution: None,
                session_id: session.id(),
                run_id: run + 1,
                contract: AGENT_LOOP.into(),
                payload: serde_json::json!({ "prompt": "stale" }),
            },
            Cancellation::default(),
        )
        .await;
    assert!(matches!(stale.outcome, Outcome::Failed(error) if error.code == "Unavailable"));
    let other_next = other.submit("still independent")?;
    assert!(matches!(
        other.wait(other_next).await?.outcome,
        Outcome::Completed(_)
    ));
    other.shutdown().await?;
    println!(
        "{}",
        serde_json::json!({
            "mode": mode,
            "terminal": terminal,
            "cleanup_gate": "held_until_release",
            "root_socket": "eof",
            "child": "joined",
            "stale_handle": "rejected",
            "other_session": "unaffected",
        })
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pair(first: &str, second: &str) -> [TcpStream; 2] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut first_client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let first_stream = listener.accept().await.unwrap().0;
        first_client.write_all(first.as_bytes()).await.unwrap();
        let mut second_client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let second_stream = listener.accept().await.unwrap().0;
        second_client.write_all(second.as_bytes()).await.unwrap();
        [first_stream, second_stream]
    }

    #[tokio::test]
    async fn matches_both_arrival_orders_and_preserves_following_bytes() {
        for greetings in [
            ["root\nR", "child\nchild-stopping\n"],
            ["child\nchild-stopping\n", "root\nR"],
        ] {
            let [mut root, mut child] = root_and_child(pair(greetings[0], greetings[1]).await)
                .await
                .unwrap();
            let mut byte = [0];
            root.read_exact(&mut byte).await.unwrap();
            assert_eq!(byte, *b"R");
            let mut line = String::new();
            child.read_line(&mut line).await.unwrap();
            assert_eq!(line, "child-stopping\n");
        }
    }

    #[tokio::test]
    async fn rejects_duplicate_and_unknown_identities() {
        for greetings in [
            ["root\n", "root\n"],
            ["child\n", "child\n"],
            ["unknown\n", "child\n"],
            ["root\n", "unknown\n"],
        ] {
            assert!(
                root_and_child(pair(greetings[0], greetings[1]).await)
                    .await
                    .is_err()
            );
        }
    }
}
