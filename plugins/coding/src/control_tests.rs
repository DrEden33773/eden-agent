use super::*;

fn record(sequence: u64, kind: &str) -> Record {
    Record {
        parent_id: sequence.checked_sub(1).filter(|id| *id > 0),
        branch: "main".into(),
        schema_version: 2,
        session_id: 1,
        sequence,
        run_id: 1,
        kind: kind.into(),
        payload: json!({ "id": 1, "branch": "main", "kind": "steering", "content": [] }),
    }
}

#[test]
fn withdrawn_input_is_not_pending_or_projected_after_reopen() {
    let records = vec![
        record(1, "queue_accepted"),
        record(2, "queue_delivered"),
        record(3, "queue_returned"),
        record(4, "queue_withdrawn"),
    ];
    assert!(pending(&records).unwrap().is_empty());
    assert!(project_records(&records).unwrap().is_empty());
    assert_eq!(queue::statuses(&records)[&1].0, "queue_withdrawn");
}

#[test]
fn queue_withdraw_wire_distinguishes_all_from_empty_selection() {
    for ids in [None, Some(vec![]), Some(vec![1, 3])] {
        let value = serde_json::to_value(QueueRequest::Withdraw { ids: ids.clone() }).unwrap();
        assert_eq!(value["operation"], "withdraw");
        let QueueRequest::Withdraw { ids: decoded } = serde_json::from_value(value).unwrap() else {
            panic!("withdraw must round trip");
        };
        assert_eq!(decoded, ids);
    }
}

#[tokio::test]
async fn stop_retry_only_wakes_current_wait_and_settings_are_instance_local() {
    let settings = Settings::default();
    let other = Settings::default();
    assert!(
        !settings
            .control(CodingControlRequest::StopRetry)
            .retry_waiting
    );
    let mut wait = settings.begin_retry().unwrap();
    assert!(
        settings
            .control(CodingControlRequest::Inspect)
            .retry_waiting
    );
    settings.control(CodingControlRequest::StopRetry);
    wait.stopped().await;
    drop(wait);
    assert!(
        !settings
            .control(CodingControlRequest::Inspect)
            .retry_waiting
    );
    let mut next = settings.begin_retry().unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(1), next.stopped())
            .await
            .is_err()
    );
    settings.control(CodingControlRequest::SetAutoRetry { enabled: false });
    next.stopped().await;
    drop(next);
    assert!(settings.begin_retry().is_none());
    assert!(other.begin_retry().is_some());
    settings.control(CodingControlRequest::SetAutoCompaction { enabled: false });
    assert!(!settings.clone().auto_compaction());
    assert!(other.auto_compaction());
}

use eden_plugin_sdk::{
    Cancellation,
    abi::{Bytes, HostApi, Reply},
    local::LocalInstance,
    protocol::{Outcome, Request, Terminal},
};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct Host {
    records: Mutex<Vec<Record>>,
    provider_calls: AtomicUsize,
    cancellations: AtomicUsize,
    retrying: tokio::sync::Notify,
}
unsafe extern "C" fn request(host: usize, bytes: Bytes, reply: Reply) -> u64 {
    // SAFETY: The test keeps its boxed host alive until the instance stop barrier.
    let host = unsafe { &*(host as *const Host) };
    // SAFETY: The SDK keeps this request span live for this synchronous callback.
    let request: Request = unsafe { bytes.decode() }.unwrap();
    let result = match request.contract.as_str() {
        STORE => {
            let mut records = host.records.lock().unwrap();
            let drafts = match serde_json::from_value(request.payload).unwrap() {
                StoreRequest::Read => vec![],
                StoreRequest::Append { kind, payload, .. } => vec![RecordDraft { kind, payload }],
                StoreRequest::AppendBatch { entries, .. } => entries,
                _ => panic!("unexpected store operation"),
            };
            for draft in drafts {
                let mut next = record(records.len() as u64 + 1, &draft.kind);
                next.payload = draft.payload;
                records.push(next);
            }
            Ok(json!(StoreReply {
                active_head: records.last().map(|r| r.sequence),
                active_branch: "main".into(),
                session_id: 1,
                sequence: records.len() as u64,
                records: records.clone(),
            }))
        }
        PROVIDER => {
            host.provider_calls.fetch_add(1, Ordering::SeqCst);
            Err(Fault::new(
                "RetryableProviderFailure",
                "fixture",
                "try later",
            ))
        }
        role => Err(Fault::new("MissingDependency", "router", role)),
    };
    let terminal = Terminal {
        outcome: match result {
            Ok(value) => Outcome::Completed(value),
            Err(error) => Outcome::Failed(error),
        },
        cleanup_errors: vec![],
        partial_result: None,
    };
    // SAFETY: Each admitted bridge owns this callback token exactly once.
    unsafe { reply.send(&terminal) };
    1
}
unsafe extern "C" fn cancel(host: usize, _: u64) {
    // SAFETY: The test host outlives every admitted callback.
    unsafe { &*(host as *const Host) }
        .cancellations
        .fetch_add(1, Ordering::SeqCst);
}
unsafe extern "C" fn event(host: usize, bytes: Bytes) {
    // SAFETY: Both host and borrowed span remain valid during this callback.
    let host = unsafe { &*(host as *const Host) };
    // SAFETY: SDK owns the borrowed event bytes until this callback returns.
    let event: Request = unsafe { bytes.decode() }.unwrap();
    if event.contract == "model_retry" {
        host.retrying.notify_one();
    }
}
fn instance(host: &Host, package: Package) -> Arc<LocalInstance> {
    // SAFETY: Tests retain the host and await stop before dropping it. Callbacks
    // copy borrowed bytes synchronously and synchronize shared state.
    unsafe {
        LocalInstance::new(
            package,
            HostApi {
                context: host as *const Host as usize,
                request,
                cancel,
                event,
            },
        )
    }
}
async fn call(
    instance: &Arc<LocalInstance>,
    role: &str,
    payload: impl serde::Serialize,
) -> Result<Value, Fault> {
    instance
        .call(
            Request {
                session_id: 1,
                run_id: 1,
                contract: role.into(),
                payload: serde_json::to_value(payload).unwrap(),
            },
            Cancellation::default(),
        )
        .await
        .into_result()
}

#[tokio::test]
async fn queue_withdraw_take_and_restore_use_durable_exclusive_states() {
    let host = Box::<Host>::default();
    let package = instance(&host, create(Value::Null).unwrap());
    for _ in 0..2 {
        call(
            &package,
            QUEUE,
            QueueRequest::Enqueue {
                kind: "steering".into(),
                content: vec![],
            },
        )
        .await
        .unwrap();
    }
    let (taken, withdrawn) = tokio::join!(
        call(
            &package,
            QUEUE,
            QueueRequest::Take {
                kind: "steering".into()
            }
        ),
        call(&package, QUEUE, QueueRequest::Withdraw { ids: None }),
    );
    let taken: Vec<QueueEntry> = serde_json::from_value(taken.unwrap()).unwrap();
    let withdrawn: Vec<QueueEntry> = serde_json::from_value(withdrawn.unwrap()).unwrap();
    assert_eq!(taken.len() + withdrawn.len(), 2);
    assert!(
        !taken
            .iter()
            .any(|entry| withdrawn.iter().any(|other| entry.id == other.id))
    );
    let restored: Vec<QueueEntry> =
        serde_json::from_value(call(&package, QUEUE, QueueRequest::Restore).await.unwrap())
            .unwrap();
    assert_eq!(
        restored.iter().map(|e| e.id).collect::<Vec<_>>(),
        taken.iter().map(|e| e.id).collect::<Vec<_>>()
    );
    call(&package, QUEUE, QueueRequest::Withdraw { ids: None })
        .await
        .unwrap();
    package.stop().await.unwrap();
    let bytes = serde_json::to_vec(&*host.records.lock().unwrap()).unwrap();
    *host.records.lock().unwrap() = serde_json::from_slice(&bytes).unwrap();
    let reopened = instance(&host, create(Value::Null).unwrap());
    assert_eq!(
        call(&reopened, QUEUE, QueueRequest::Restore).await.unwrap(),
        json!([])
    );
    assert!(
        !host
            .records
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.kind == "queue_consumed")
    );
    reopened.stop().await.unwrap();
}

#[tokio::test]
async fn stop_retry_returns_original_failure_without_cancelling_scope_or_next_call() {
    let host = Box::<Host>::default();
    let settings = Settings::parse(json!({ "retry": { "base_delay_ms": 60000 } })).unwrap();
    let control_settings = settings.clone();
    let package = Package::new("fixture")
        .service("retry", move |input: ModelInput, cx| {
            let settings = settings.clone();
            async move { provider_retry(&cx, &input, &settings).await }
        })
        .service(CODING_CONTROL, move |request, _| {
            let state = control_settings.control(request);
            async move { Ok(state) }
        });
    let package = instance(&host, package);
    let input = ModelInput {
        target: None,
        max_output_tokens: None,
        items: vec![],
        tools: vec![],
    };
    let pending = call(&package, "retry", input.clone());
    let control = async {
        host.retrying.notified().await;
        call(&package, CODING_CONTROL, CodingControlRequest::StopRetry)
            .await
            .unwrap();
    };
    let (result, ()) = tokio::join!(pending, control);
    assert_eq!(result.unwrap_err().code, "RetryableProviderFailure");
    assert_eq!(host.provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(host.cancellations.load(Ordering::SeqCst), 0);
    call(
        &package,
        CODING_CONTROL,
        CodingControlRequest::SetAutoRetry { enabled: false },
    )
    .await
    .unwrap();
    assert_eq!(
        call(&package, "retry", input).await.unwrap_err().code,
        "RetryableProviderFailure"
    );
    assert_eq!(host.provider_calls.load(Ordering::SeqCst), 2);
    package.stop().await.unwrap();
}

#[test]
fn exported_descriptor_matches_factory_roles() {
    assert_eq!(create(Value::Null).unwrap().descriptor(), &descriptor());
}

#[tokio::test]
async fn changed_delivery_mode_applies_to_next_take_without_rewriting_delivered_entries() {
    let host = Box::<Host>::default();
    let package = instance(&host, create(Value::Null).unwrap());
    for _ in 0..3 {
        call(
            &package,
            QUEUE,
            QueueRequest::Enqueue {
                kind: "steering".into(),
                content: vec![],
            },
        )
        .await
        .unwrap();
    }
    let first: Vec<QueueEntry> = serde_json::from_value(
        call(
            &package,
            QUEUE,
            QueueRequest::Take {
                kind: "steering".into(),
            },
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(first.len(), 1);
    call(
        &package,
        QUEUE,
        QueueRequest::Configure {
            steering: "all".into(),
            follow_up: "one".into(),
        },
    )
    .await
    .unwrap();
    let next: Vec<QueueEntry> = serde_json::from_value(
        call(
            &package,
            QUEUE,
            QueueRequest::Take {
                kind: "steering".into(),
            },
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(next.len(), 2);
    assert!(next.iter().all(|entry| entry.id != first[0].id));
    let records = host.records.lock().unwrap().clone();
    assert_eq!(
        records
            .iter()
            .filter(|r| r.kind == "queue_delivered" && r.payload["id"] == first[0].id)
            .count(),
        1
    );
    package.stop().await.unwrap();
}
