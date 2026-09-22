//! Exercise memory-session delivery through the public embedding facade.
use eden_agent::{Session, WorkspaceOptions};
use eden_protocol::delivery::{Format, Selection};
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let composition = std::env::args_os().nth(1).ok_or("composition required")?;
    let cwd = std::env::current_dir()?;
    let options = WorkspaceOptions {
        global_dir: cwd.join("sdk-global"),
        overrides: serde_json::json!({ "offline_startup": true }),
        ..Default::default()
    };
    let session = Session::open_with_workspace(
        composition,
        eden_agent::SessionOptions { cwd, history: None },
        options,
    )
    .await?;
    let result = session.export(Selection::default(), Format::Html).await;
    let stopped = session.shutdown().await;
    let artifact = result?;
    stopped?;
    assert!(artifact.content.contains("Conversation"));
    assert!(!artifact.content.contains("composition_lock"));
    println!("{}", serde_json::json!({ "memory_snapshot": true }));
    Ok(())
}
