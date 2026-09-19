//! Installed session probe for an abandoned store-open delivery.
//!
//! Queue, steering and cancellation behaviour is covered by the installed
//! context probe and the session suites; this binary keeps the one lifecycle
//! case that needs a real competing writer.
use eden_agent::{Session, SessionOptions};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let options = SessionOptions {
        cwd: PathBuf::from(&args[2]),
        history: Some(PathBuf::from(&args[3])),
    };
    if args[4] != "open-abandon" {
        return Err(format!("unsupported coding probe mode: {}", args[4]).into());
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        open_abandon(&args[1], options),
    )
    .await?
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
            package["config"] = serde_json::json!({
                "open_gate": listener.local_addr()?.to_string(),
            });
        }
    }
    std::fs::write(composition_path, serde_json::to_vec(&composition)?)?;
    let path = composition_path.to_owned();
    let opening_options = options.clone();
    let mut opening = tokio::spawn(async move { Session::open_with(path, opening_options).await });
    let (stream, _) = tokio::select! {
        accepted = listener.accept() => accepted?,
        completed = &mut opening => {
            let session = completed??;
            session.shutdown().await?;
            return Err("session initialized without reaching the STORE.Open gate".into());
        }
    };
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
    assert_eq!(
        reopened
            .history()
            .await?
            .iter()
            .map(|r| r.kind.as_str())
            .collect::<Vec<_>>(),
        ["session", "composition_lock"]
    );
    reopened.shutdown().await?;
    println!(
        "{}",
        serde_json::json!({
            "open_lock_observed": true,
            "caller_aborted": true,
            "store_close_acknowledged": true,
            "reopened_after_abandoned_delivery": true,
        })
    );
    Ok(())
}
