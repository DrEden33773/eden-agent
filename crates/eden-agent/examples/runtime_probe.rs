//! Installed native author acceptance for runtime ownership and cleanup barriers.
use eden_kernel::{Events, Kernel};
use eden_plugin_sdk::Cancellation;
use eden_protocol::{self as p, Request};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
const CONTROL: &str = "author.runtime.control.v1";
async fn control(
    kernel: &Kernel,
    owner: &str,
    run_id: u64,
    payload: Value,
) -> Result<Value, p::Fault> {
    kernel
        .invoke_package(
            owner,
            Request {
                execution: None,
                session_id: 1,
                run_id,
                contract: CONTROL.into(),
                payload,
            },
            Cancellation::default(),
        )
        .await
        .into_result()
}
async fn run(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let events = Events::new(1);
    let kernel = Arc::new(Kernel::load(path, 1, events.clone()).await?);
    assert_eq!(
        events
            .snapshot()
            .iter()
            .filter(|e| e.kind == "author_ready")
            .count(),
        5
    );
    let role = kernel.role(p::CONTEXT)?;
    let call = |input: &str| Request {
        execution: None,
        session_id: 1,
        run_id: 1,
        contract: p::CONTEXT.into(),
        payload: json!({ "prompt": input }),
    };
    assert_eq!(
        role.call(call("x"), Cancellation::default())
            .await
            .into_result()?["text"],
        json!("one[two[standard:two(one(x))]]")
    );
    assert_eq!(
        role.call(call("short"), Cancellation::default())
            .await
            .into_result()?["text"],
        json!("one:short")
    );
    assert_eq!(
        role.call(call("fail"), Cancellation::default())
            .await
            .into_result()
            .unwrap_err()
            .source,
        "two"
    );
    assert_eq!(
        control(
            &kernel,
            "worker",
            1,
            json!({ "op": "scoped", "scope": "a" })
        )
        .await?,
        json!("a(x)")
    );
    assert_eq!(
        control(
            &kernel,
            "worker",
            1,
            json!({ "op": "scoped", "scope": "b" })
        )
        .await?,
        json!("one[two[standard:two(one(x))]]")
    );
    let before = kernel.instance_identity("one")?;
    let a = kernel.instance_identity("child-a")?;
    let b = kernel.instance_identity("child-b")?;
    assert_ne!(a.generation, b.generation);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let registered = control(
        &kernel,
        "worker",
        1,
        json!({ "op": "start", "endpoint": listener.local_addr()?.to_string() }),
    )
    .await?;
    let job = registered["id"].as_u64().ok_or("missing job")?;
    assert!(registered["terminal"].is_null());
    let (mut stream, _) = listener.accept().await?;
    let mut ready = [0; 5];
    stream.read_exact(&mut ready).await?;
    assert_eq!(&ready, b"ready");
    events.push(2, "next_input", json!({ "text": "second turn" }));
    let mut cursor = 0;
    loop {
        let batch = events.read_after(cursor).await?;
        cursor = batch.last().map_or(cursor, |e| e.sequence);
        if let Some(event) = batch.iter().find(|e| e.kind == "job_observed_input") {
            assert_eq!(event.run_id, 0);
            assert_eq!(event.payload["input_run"], 2);
            assert_eq!(event.payload["identity"]["job"], job);
            break;
        }
    }
    assert!(
        control(&kernel, "worker", 2, json!({ "op": "inspect", "job": job })).await?["terminal"]
            .is_null()
    );
    control(&kernel, "worker", 2, json!({ "op": "cancel", "job": job })).await?;
    let joining = kernel.clone();
    let waiter = tokio::spawn(async move {
        control(&joining, "worker", 2, json!({ "op": "join", "job": job })).await
    });
    let mut cleanup = [0; 7];
    stream.read_exact(&mut cleanup).await?;
    assert_eq!(&cleanup, b"cleanup");
    assert!(
        !waiter.is_finished(),
        "join returned before native TCP cleanup acknowledgement"
    );
    stream.write_all(b"ack").await?;
    let status: p::runtime::JobStatus = serde_json::from_value(waiter.await??)?;
    let terminal = status.terminal.ok_or("missing settled terminal")?;
    assert_eq!(terminal.outcome, p::Outcome::Cancelled);
    assert!(terminal.cleanup_errors.is_empty());
    let mut eof = [0; 1];
    assert_eq!(stream.read(&mut eof).await?, 0);
    // Instance shutdown owns cancellation and cannot pass the native cleanup barrier early.
    let next = control(
        &kernel,
        "worker",
        3,
        json!({ "op": "start", "endpoint": listener.local_addr()?.to_string() }),
    )
    .await?;
    assert!(next["terminal"].is_null());
    let (mut closing_stream, _) = listener.accept().await?;
    closing_stream.read_exact(&mut ready).await?;
    let stopping = kernel.clone();
    let stopped = tokio::spawn(async move { stopping.stop_instance("worker").await });
    closing_stream.read_exact(&mut cleanup).await?;
    assert_eq!(&cleanup, b"cleanup");
    assert!(
        !stopped.is_finished(),
        "instance stop returned before job cleanup"
    );
    closing_stream.write_all(b"ack").await?;
    stopped.await??;
    assert_eq!(closing_stream.read(&mut eof).await?, 0);
    assert!(
        control(&kernel, "worker", 3, json!({ "op": "identity" }))
            .await
            .is_err()
    );
    assert_eq!(kernel.instance_identity("one")?, before);
    assert!(
        role.call(call("x"), Cancellation::default())
            .await
            .into_result()
            .is_ok()
    );
    kernel.shutdown().await?;
    assert_eq!(
        role.call(call("x"), Cancellation::default())
            .await
            .into_result()
            .unwrap_err()
            .code,
        "Unavailable"
    );
    let cwd = std::env::current_dir()?;
    let global_dir = cwd.join("runtime-global");
    let session = eden_agent::Session::open_with_workspace(
        path,
        eden_agent::SessionOptions {
            cwd: cwd.clone(),
            history: None,
        },
        eden_agent::WorkspaceOptions {
            global_dir: global_dir.clone(),
            project_trust: Some(false),
            overrides: json!({}),
        },
    )
    .await?;
    let environment = session
        .role(CONTROL)?
        .call(
            Request {
                execution: None,
                session_id: session.id(),
                run_id: 0,
                contract: CONTROL.into(),
                payload: json!({ "op": "environment" }),
            },
            Cancellation::default(),
        )
        .await
        .into_result()?;
    // Workspace discovery resolves Windows short names and verbatim path prefixes.
    assert_eq!(environment["cwd"], json!(std::fs::canonicalize(&cwd)?));
    assert_eq!(environment["global_dir"], json!(global_dir));
    assert_eq!(environment["project_trusted"], false);
    session.shutdown().await?;
    println!(
        "{}",
        json!({
            "PL-01": "native wrappers, single-use continuation, short circuit, error source",
            "PL-02":
                "same-library instances, parent inheritance, child override, sibling isolation",
            "PL-04": "native author receives authoritative workspace environment",
            "PL-03":
                "cross-turn event, run-zero job, TCP cleanup acknowledgement, stale handle rejected",
        })
    );
    Ok(())
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("composition required")?;
    tokio::time::timeout(std::time::Duration::from_secs(40), run(Path::new(&path))).await?
}
