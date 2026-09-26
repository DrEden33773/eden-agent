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
    let environment: p::environment::HostEnvironment = serde_json::from_value(environment)?;
    // Compare paths, not JSON spellings: Windows accepts both separators and short names.
    assert_eq!(environment.cwd, std::fs::canonicalize(&cwd)?);
    assert_eq!(environment.global_dir, global_dir);
    assert!(!environment.project_trusted);
    let first = session.submit(format!("job@{}", listener.local_addr()?))?;
    session.wait(first).await?.into_result()?;
    let (mut session_stream, _) = listener.accept().await?;
    session_stream.read_exact(&mut ready).await?;
    assert_eq!(&ready, b"ready");
    let second = session.submit("second turn")?;
    session.wait(second).await?.into_result()?;
    let mut cursor = 0;
    let mut registered_job = None;
    loop {
        let batch = session.read_events(cursor).await?;
        cursor = batch.last().map_or(cursor, |event| event.sequence);
        for event in &batch {
            if event.kind == "author_job_registered" && event.run_id == first {
                registered_job = event.payload["id"].as_u64();
            }
        }
        if let Some(event) = batch.iter().find(|event| {
            event.kind == "job_observed_input" && event.payload["input_run"] == second
        }) {
            assert_eq!(event.run_id, 0);
            assert_eq!(event.payload["identity"]["owner"]["id"], "one");
            assert_eq!(event.payload["identity"]["job"].as_u64(), registered_job);
            assert!(registered_job.is_some());
            break;
        }
    }
    let closing_session = session.clone();
    let closed = tokio::spawn(async move { closing_session.shutdown().await });
    session_stream.read_exact(&mut cleanup).await?;
    assert_eq!(&cleanup, b"cleanup");
    assert!(
        !closed.is_finished(),
        "Session closed before its cross-turn job cleaned up"
    );
    session_stream.write_all(b"ack").await?;
    closed.await??;
    assert_eq!(session_stream.read(&mut eof).await?, 0);
    dependency_acceptance(path).await?;
    configuration_acceptance(path).await?;
    println!(
        "{}",
        json!({
            "LR-01": "native local restart; unrelated incarnation, job and event subscription retained",
            "LR-02":
                "native wrapper dependents, explicit dependencies and owned child restart; \
                 isolated sibling retained",
            "LR-03":
                "unrelated restart during foreground call; safe-boundary wait and explicit \
                 cancellation await TCP cleanup",
            "LR-04":
                "validation preserves old state; prepare failure restores once; failed recovery \
                 counted; cleanup blocks successors",
            "LR-05":
                "immutable library path replacement retains unrelated standard instance; history \
                 checked separately",
            "LR-06":
                "native Describe/Validate/Update applies live field without changing generation",
            "PL-01":
                "native wrappers, single-use continuation, short circuit, error source",
            "PL-02":
                "same-library instances, parent inheritance, child override, sibling isolation",
            "PL-04": "native author receives authoritative workspace environment",
            "PL-03":
                "two settled Session turns, run-zero job, TCP cleanup acknowledgement, stale \
                 handle rejected",
        })
    );
    Ok(())
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("composition required")?;
    tokio::time::timeout(std::time::Duration::from_secs(40), run(Path::new(&path))).await?
}

async fn configuration_acceptance(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use eden_agent::configuration::{Application, ApplyMode, Change, Status};
    let session = eden_agent::Session::open(path).await?;
    let inspect = session.inspect_configuration().await?;
    let identity = |inspect: &eden_agent::configuration::Inspection, id: &str| {
        inspect
            .instances
            .iter()
            .find(|instance| instance.id == id)
            .and_then(|instance| instance.generation)
            .unwrap()
    };
    let change = |instance: &str, revision, patch| Change {
        instance: instance.into(),
        revision,
        patch,
        replacement: None,
    };
    let original_worker = identity(&inspect, "worker");
    let original_child = identity(&inspect, "child-a");
    let bad = change("child-a", inspect.revision, json!({ "label": "forbidden" }));
    assert_eq!(
        session.validate_configuration(bad.clone()).await?.errors[0].code,
        "author_label"
    );
    assert_eq!(
        session
            .apply_configuration(bad, ApplyMode::Wait)
            .await
            .unwrap_err()
            .code,
        "ValidationFailed"
    );
    let live = change("child-a", inspect.revision, json!({ "label": "live" }));
    assert_eq!(
        session
            .preview_configuration(live.clone())
            .await?
            .application,
        Application::Live
    );
    let operation = session.apply_configuration(live, ApplyMode::Wait).await?;
    assert_eq!(
        session.wait_configuration(operation).await?.status,
        Status::Applied
    );
    let inspect = session.inspect_configuration().await?;
    assert_eq!(identity(&inspect, "child-a"), original_child);
    assert_eq!(identity(&inspect, "worker"), original_worker);
    let control = session.role(CONTROL)?;
    let call = |payload| Request {
        execution: None,
        session_id: session.id(),
        run_id: 0,
        contract: CONTROL.into(),
        payload,
    };
    assert_eq!(
        control
            .call(
                call(json!({ "op": "scoped", "scope": "a" })),
                Cancellation::default()
            )
            .await
            .into_result()?,
        json!("live(x)")
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    control
        .call(
            call(json!({ "op": "start", "endpoint": listener.local_addr()?.to_string() })),
            Cancellation::default(),
        )
        .await
        .into_result()?;
    let (mut stream, _) = listener.accept().await?;
    let mut ready = [0; 5];
    stream.read_exact(&mut ready).await?;
    let restart = change("worker", inspect.revision, json!({ "restart_marker": 1 }));
    let preview = session.preview_configuration(restart.clone()).await?;
    assert_eq!(preview.affected, vec!["worker"]);
    assert_eq!(preview.application, Application::Restart);
    assert_eq!(
        preview
            .jobs
            .iter()
            .filter(|job| job.terminal.is_none())
            .count(),
        1
    );
    let operation = session
        .apply_configuration(restart, ApplyMode::Wait)
        .await?;
    let mut cleanup = [0; 7];
    stream.read_exact(&mut cleanup).await?;
    assert_eq!(&cleanup, b"cleanup");
    assert_eq!(
        session.configuration_operation(operation)?.status,
        Status::Applying
    );
    stream.write_all(b"ack").await?;
    assert_eq!(
        session.wait_configuration(operation).await?.status,
        Status::Applied
    );
    assert!(
        control
            .call(call(json!({ "op": "identity" })), Cancellation::default())
            .await
            .into_result()
            .is_err()
    );
    let inspect = session.inspect_configuration().await?;
    assert_ne!(identity(&inspect, "worker"), original_worker);
    assert_eq!(identity(&inspect, "child-a"), original_child);

    // An unrelated replacement can commit while a real foreground native request is blocked.
    let run = session.submit(format!("hold@{}", listener.local_addr()?))?;
    let (mut foreground, _) = listener.accept().await?;
    foreground.read_exact(&mut ready).await?;
    let unrelated = change("worker", inspect.revision, json!({ "restart_marker": 2 }));
    assert!(
        session
            .preview_configuration(unrelated.clone())
            .await?
            .waiting_runs
            .is_empty()
    );
    let operation = session
        .apply_configuration(unrelated, ApplyMode::Wait)
        .await?;
    assert_eq!(
        session.wait_configuration(operation).await?.status,
        Status::Applied
    );
    let inspect = session.inspect_configuration().await?;
    let waiting = change("one", inspect.revision, json!({ "restart_marker": 1 }));
    assert_eq!(
        session
            .preview_configuration(waiting.clone())
            .await?
            .waiting_runs,
        vec![run]
    );
    let operation = session
        .apply_configuration(waiting, ApplyMode::Wait)
        .await?;
    assert_eq!(
        session.configuration_operation(operation)?.status,
        Status::Waiting
    );
    foreground.write_all(b"go!").await?;
    foreground.read_exact(&mut cleanup).await?;
    assert_eq!(&cleanup, b"cleanup");
    assert_eq!(
        session.configuration_operation(operation)?.status,
        Status::Waiting
    );
    foreground.write_all(b"ack").await?;
    session.wait(run).await?.into_result()?;
    assert_eq!(
        session.wait_configuration(operation).await?.status,
        Status::Applied
    );
    let run = session.submit(format!("hold@{}", listener.local_addr()?))?;
    let (mut foreground, _) = listener.accept().await?;
    foreground.read_exact(&mut ready).await?;
    let inspect = session.inspect_configuration().await?;
    let operation = session
        .apply_configuration(
            change("one", inspect.revision, json!({ "restart_marker": 2 })),
            ApplyMode::Cancel,
        )
        .await?;
    foreground.read_exact(&mut cleanup).await?;
    assert_eq!(&cleanup, b"cleanup");
    assert_eq!(
        session.configuration_operation(operation)?.status,
        Status::Waiting
    );
    foreground.write_all(b"ack").await?;
    assert_eq!(session.wait(run).await?.outcome, p::Outcome::Cancelled);
    assert_eq!(
        session.wait_configuration(operation).await?.status,
        Status::Applied
    );

    // Metadata validation accepts this configuration; only native create rejects it.
    let inspect = session.inspect_configuration().await?;
    let prepare_failure = change("child-a", inspect.revision, json!({ "fail_prepare": true }));
    assert!(
        session
            .validate_configuration(prepare_failure.clone())
            .await?
            .errors
            .is_empty()
    );
    let operation = session
        .apply_configuration(prepare_failure, ApplyMode::Wait)
        .await?;
    let receipt = session.wait_configuration(operation).await?;
    assert_eq!(receipt.status, Status::Restored);
    assert_eq!(receipt.error.unwrap().code, "InitializationFailure");
    assert!(receipt.recovery_error.is_none());
    let inspect = session.inspect_configuration().await?;
    assert_eq!(
        inspect
            .instances
            .iter()
            .find(|instance| instance.id == "child-a")
            .unwrap()
            .effective["label"],
        "live"
    );

    // Code identity changes use a separate immutable library path, avoiding loader path caching.
    let mut replacement: p::Composition = serde_json::from_slice(&std::fs::read(path)?)?;
    for manifest in &mut replacement.packages {
        let library = std::fs::canonicalize(
            path.parent()
                .unwrap_or(Path::new("."))
                .join(&manifest.library),
        )?;
        manifest.library = library.to_string_lossy().into_owned();
        if manifest.descriptor.package == "runtime-author" {
            let copied = std::env::current_dir()?.join(format!(
                "replacement-{}",
                library.file_name().unwrap().to_string_lossy()
            ));
            std::fs::copy(&library, &copied)?;
            manifest.library = copied.to_string_lossy().into_owned();
        }
    }
    let replacement_path = std::env::current_dir()?.join("runtime-replacement.json");
    std::fs::write(&replacement_path, serde_json::to_vec(&replacement)?)?;
    let mut replace = change("worker", inspect.revision, json!({}));
    replace.replacement = Some(replacement_path);
    let preview = session.preview_configuration(replace.clone()).await?;
    assert_eq!(preview.affected.len(), 5);
    let standard = identity(&inspect, "standard");
    let operation = session
        .apply_configuration(replace, ApplyMode::Wait)
        .await?;
    assert_eq!(
        session.wait_configuration(operation).await?.status,
        Status::Applied
    );
    let inspect = session.inspect_configuration().await?;
    assert_eq!(identity(&inspect, "standard"), standard);

    // One failed candidate and exactly one failed restoration are observable in a private counter.
    let attempts = std::env::current_dir()?.join("runtime-prepare-attempts");
    let operation = session
        .apply_configuration(
            change(
                "child-b",
                inspect.revision,
                json!({ "attempts_file": attempts, "fail_after": 1 }),
            ),
            ApplyMode::Wait,
        )
        .await?;
    assert_eq!(
        session.wait_configuration(operation).await?.status,
        Status::Applied
    );
    let inspect = session.inspect_configuration().await?;
    let operation = session
        .apply_configuration(
            change("child-b", inspect.revision, json!({ "fail_prepare": true })),
            ApplyMode::Wait,
        )
        .await?;
    let receipt = session.wait_configuration(operation).await?;
    assert_eq!(receipt.status, Status::RecoveryFailed);
    assert!(receipt.recovery_error.is_some());
    assert_eq!(std::fs::read_to_string(&attempts)?, "3");
    let inspect = session.inspect_configuration().await?;
    assert_eq!(
        inspect
            .instances
            .iter()
            .find(|instance| instance.id == "child-b")
            .unwrap()
            .state,
        "unavailable"
    );
    assert_eq!(identity(&inspect, "standard"), standard);
    let operation = session
        .apply_configuration(
            change("child-a", inspect.revision, json!({ "fail_stop": true })),
            ApplyMode::Wait,
        )
        .await?;
    assert_eq!(
        session.wait_configuration(operation).await?.status,
        Status::Applied
    );
    let inspect = session.inspect_configuration().await?;
    let child = identity(&inspect, "child-a");
    let operation = session
        .apply_configuration(
            change("child-a", inspect.revision, json!({ "restart_marker": 3 })),
            ApplyMode::Wait,
        )
        .await?;
    assert_eq!(
        session.wait_configuration(operation).await?.status,
        Status::CleanupFailed
    );
    let inspect = session.inspect_configuration().await?;
    assert_eq!(identity(&inspect, "child-a"), child);
    assert_eq!(
        inspect
            .instances
            .iter()
            .find(|instance| instance.id == "child-a")
            .unwrap()
            .state,
        "unavailable"
    );
    assert!(session.shutdown().await.is_err());
    Ok(())
}

async fn dependency_acceptance(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut composition: p::Composition = serde_json::from_slice(&std::fs::read(path)?)?;
    for spec in &mut composition.runtime.instances {
        if spec.id == "worker" {
            spec.owner = Some("two".into());
        }
        if spec.id == "child-b" {
            spec.dependencies = vec!["two".into()];
        }
    }
    let events = Events::new(1);
    let kernel = Kernel::load_resolved(
        composition,
        path.parent().unwrap_or(Path::new(".")),
        1,
        events.clone(),
    )
    .await?;
    let unaffected = kernel.instance_identity("child-a")?;
    let standard = kernel.instance_identity("standard")?;
    let old = kernel.role(p::CONTEXT)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let job = control(
        &kernel,
        "child-a",
        0,
        json!({ "op": "start", "endpoint": listener.local_addr()?.to_string() }),
    )
    .await?;
    let job = job["id"].as_u64().unwrap();
    let (mut stream, _) = listener.accept().await?;
    let mut ready = [0; 5];
    stream.read_exact(&mut ready).await?;
    let mut candidate = kernel.composition();
    candidate
        .runtime
        .instances
        .iter_mut()
        .find(|instance| instance.id == "two")
        .unwrap()
        .config = Some(json!({ "mode": "wrapper", "label": "replacement" }));
    let affected = kernel.affected_instances(&candidate)?;
    assert_eq!(
        affected
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        ["two", "one", "worker", "child-b"]
            .into_iter()
            .map(String::from)
            .collect()
    );
    kernel.set_reconfiguring(&affected, true)?;
    kernel.replace_instances(candidate, &affected).await?;
    kernel.set_reconfiguring(&affected, false)?;
    assert_eq!(kernel.instance_identity("child-a")?, unaffected);
    assert_eq!(kernel.instance_identity("standard")?, standard);
    let retained = kernel
        .jobs()
        .into_iter()
        .find(|entry| entry.id == job)
        .unwrap();
    assert_eq!(retained.owner, unaffected);
    assert!(retained.terminal.is_none());
    let request = Request {
        execution: None,
        session_id: 1,
        run_id: 1,
        contract: p::CONTEXT.into(),
        payload: json!({ "prompt": "x" }),
    };
    assert!(
        old.call(request.clone(), Cancellation::default())
            .await
            .into_result()
            .is_err()
    );
    assert_eq!(
        kernel
            .invoke(request, Cancellation::default())
            .await
            .into_result()?["text"],
        "one[replacement[standard:replacement(one(x))]]"
    );
    events.push(2, "next_input", json!({}));
    let mut cursor = 0;
    loop {
        let batch = events.read_after(cursor).await?;
        cursor = batch.last().map_or(cursor, |event| event.sequence);
        if let Some(event) = batch
            .iter()
            .find(|event| event.kind == "job_observed_input")
        {
            assert_eq!(
                event.payload["identity"]["owner"]["generation"],
                unaffected.generation
            );
            assert_eq!(event.payload["identity"]["job"], job);
            break;
        }
    }
    control(&kernel, "child-a", 0, json!({ "op": "cancel", "job": job })).await?;
    let mut cleanup = [0; 7];
    stream.read_exact(&mut cleanup).await?;
    assert_eq!(&cleanup, b"cleanup");
    stream.write_all(b"ack").await?;
    control(&kernel, "child-a", 0, json!({ "op": "join", "job": job })).await?;
    kernel.shutdown().await?;
    Ok(())
}
