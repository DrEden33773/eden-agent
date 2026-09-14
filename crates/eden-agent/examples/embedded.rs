use eden_agent::Session;
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: embedded COMPOSITION")?;
    let session = Session::open(path).await?;
    let result = async {
        let run = session.submit("hello")?;
        let terminal = session.wait(run).await?;
        println!("{}", serde_json::to_string(&terminal)?);
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    let shutdown = session.shutdown().await;
    result?;
    shutdown?;
    Ok(())
}
