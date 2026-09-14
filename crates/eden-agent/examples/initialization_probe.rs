//! Proves abandoned initialization finishes rollback before destroying native resources.
use eden_agent::Session;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::path::PathBuf::from(std::env::args().nth(1).ok_or("composition required")?);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let mut composition: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    for package in composition["packages"]
        .as_array_mut()
        .ok_or("packages required")?
    {
        if package["descriptor"]["package"] == "init-probe" {
            package["config"] = serde_json::json!({"address": listener.local_addr()?.to_string()});
        }
    }
    let pending_path = path.with_file_name("abandoned-init.json");
    std::fs::write(&pending_path, serde_json::to_vec(&composition)?)?;
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let opening = tokio::spawn(async move { Session::open(pending_path).await });
        let (stream, _) = listener.accept().await?;
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream.read_line(&mut line).await?;
        assert_eq!(line.trim(), "initializing");
        opening.abort();
        assert!(matches!(opening.await, Err(error) if error.is_cancelled()));
        stream.write_all(b"G").await?;
        line.clear();
        stream.read_line(&mut line).await?;
        assert_eq!(line.trim(), "destroyed");
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await??;
    println!("{{\"abandoned_initialization\":\"destroyed\"}}");
    Ok(())
}
