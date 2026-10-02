//! Public runtime behavior shared by local and native authors.
#![allow(missing_docs)]
use eden_kernel::{Events, Kernel};
use eden_plugin_sdk::{Cancellation, Package};
use eden_protocol::{self as p, Request};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

async fn mount(packages: Vec<Package>, graph: Value) -> Kernel {
    mount_events(packages, graph, Events::new(1)).await
}
async fn mount_events(
    packages: Vec<Package>,
    graph: Value,
    events: std::sync::Arc<Events>,
) -> Kernel {
    let manifests: Vec<_> = packages
        .iter()
        .map(|package| {
            json!({
                "descriptor": package.descriptor(),
                "host": p::CONTRACT,
                "sdk": p::CONTRACT,
                "target": eden_plugin_sdk::abi::TARGET,
                "library": "embedded",
                "config": {},
            })
        })
        .collect();
    let local = packages
        .into_iter()
        .map(|p| (p.descriptor().package.clone(), p))
        .collect();
    let roles: BTreeMap<_, _> = [p::AGENT_LOOP, p::CONTEXT, p::PROVIDER, p::TOOL]
        .into_iter()
        .map(|r| (r, "tail"))
        .collect();
    let composition = serde_json::from_value(json!({
        "packages": manifests,
        "roles": roles,
        "runtime": graph,
    }))
    .unwrap();
    Kernel::load_embedded(composition, Path::new("."), 1, events, local)
        .await
        .unwrap()
}
fn tail() -> Package {
    let mut package = Package::new("tail");
    for role in [p::AGENT_LOOP, p::CONTEXT, p::PROVIDER, p::TOOL] {
        package = package.service(role, |input: String, _| async move {
            Ok(format!("tail({input})"))
        });
    }
    package
}
#[tokio::test]
async fn optional_views_never_bypass_a_replacement_wrapper_or_scoped_store() {
    use p::coding::{STORE, STORE_ACCESS};
    for (wrapped, other_view) in [(false, false), (true, false), (false, true)] {
        let make_store = |name| {
            Package::new(name)
                .service(STORE, |_: Value, _| async { Ok(Value::Null) })
                .service(STORE_ACCESS, |_: Value, _| async { Ok(Value::Null) })
        };
        let wrapper = Package::new("store-wrapper").service(STORE, |input: Value, cx| async move {
            cx.delegate::<_, Value>(&input).await
        });
        let probe = Package::new("probe")
            .service("view-probe", |_: Value, cx| async move {
                let child: bool = cx
                    .call_in("child", "child-view-probe", &Value::Null)
                    .await?;
                Ok(json!({
                    "view": cx.has_companion(STORE, STORE_ACCESS).await?,
                    "own": cx.is_own_service("view-probe").await?,
                    "missing": cx.has_companion(STORE, "missing-view").await?,
                    "child": child,
                }))
            })
            .service("child-view-probe", |_: Value, cx| async move {
                cx.has_companion(STORE, STORE_ACCESS).await
            });
        let kernel = mount(
            vec![
                tail(),
                make_store("store"),
                make_store("other"),
                wrapper,
                probe,
            ],
            json!({
                "scopes": {
                    "": {
                        "bindings": {
                            (STORE): {
                                "tail": "store",
                                "wrappers": if wrapped {
                                        vec!["store-wrapper"]
                                    } else {
                                        vec![]
                                    },
                            },
                            (STORE_ACCESS): {
                                "tail": if other_view {
                                        "other"
                                    } else {
                                        "store"
                                    },
                            },
                            "view-probe": { "tail": "probe" },
                            "child-view-probe": { "tail": "probe" },
                        },
                    },
                    "child": { "parent": "", "bindings": { (STORE): { "tail": "other" } } },
                },
            }),
        )
        .await;
        assert_eq!(
            kernel.has_companion(STORE, STORE_ACCESS),
            !wrapped && !other_view
        );
        let result = kernel
            .invoke(
                Request {
                    execution: None,
                    session_id: 1,
                    run_id: 1,
                    contract: "view-probe".into(),
                    payload: Value::Null,
                },
                Cancellation::default(),
            )
            .await
            .into_result()
            .unwrap();
        assert_eq!(
            result,
            json!({
                "view": !wrapped && !other_view,
                "own": true,
                "missing": false,
                "child": other_view,
            })
        );
        kernel.quiesce_instance("store").unwrap();
        assert!(!kernel.has_companion(STORE, STORE_ACCESS));
        kernel.shutdown().await.unwrap();
    }
}
async fn invoke(kernel: &Kernel, input: &str) -> String {
    kernel
        .invoke(
            serde_json::from_value::<Request>(json!({
                "session_id": 1,
                "run_id": 1,
                "contract": p::CONTEXT,
                "payload": input,
            }))
            .unwrap(),
            Cancellation::default(),
        )
        .await
        .into_result()
        .unwrap()
        .as_str()
        .unwrap()
        .into()
}
#[tokio::test]
async fn pl01_wrappers_delegate_once_and_short_circuit_real_tail() {
    let first = Package::new("first").service(p::CONTEXT, |input: String, cx| async move {
        if input == "short" {
            return Ok("short".to_owned());
        }
        let output: String = cx.delegate(&format!("first({input})")).await?;
        assert_eq!(
            cx.delegate::<_, String>(&input).await.unwrap_err().code,
            "ExpiredContinuation"
        );
        Ok(format!("first[{output}]"))
    });
    let second = Package::new("second").service(p::CONTEXT, |input: String, cx| async move {
        let output: String = cx.delegate(&format!("second({input})")).await?;
        Ok(format!("second[{output}]"))
    });
    let kernel = mount(
        vec![tail(), first, second],
        json!({
            "scopes": {
                "": {
                    "bindings": {
                        (p::CONTEXT): { "tail": "tail", "wrappers": ["first", "second"] },
                    },
                },
            },
        }),
    )
    .await;
    assert_eq!(
        invoke(&kernel, "x").await,
        "first[second[tail(second(first(x)))]]"
    );
    assert_eq!(invoke(&kernel, "short").await, "short");
    kernel.shutdown().await.unwrap();
}
#[tokio::test]
async fn pl02_scope_override_does_not_escape_to_sibling() {
    let child = Package::new("child").service(p::CONTEXT, |input: String, _| async move {
        Ok(format!("child({input})"))
    });
    let caller = Package::new("caller").service("test.scopes", |_: (), cx| async move {
        let a: String = cx.call_in("a", p::CONTEXT, &"x").await?;
        let b: String = cx.call_in("b", p::CONTEXT, &"x").await?;
        Ok(vec![a, b])
    });
    let kernel = mount(
        vec![tail(), child, caller],
        json!({
            "scopes": {
                "": { "bindings": { "test.scopes": { "tail": "caller" } } },
                "a": { "parent": "", "bindings": { (p::CONTEXT): { "tail": "child" } } },
                "b": { "parent": "" },
            },
        }),
    )
    .await;
    let result = kernel
        .invoke(
            serde_json::from_value(json!({
                "session_id": 1,
                "run_id": 1,
                "contract": "test.scopes",
                "payload": null,
            }))
            .unwrap(),
            Cancellation::default(),
        )
        .await
        .into_result()
        .unwrap();
    assert_eq!(result, json!(["child(x)", "tail(x)"]));
    let captured = kernel.role(p::CONTEXT).unwrap();
    let injected = serde_json::from_value(json!({
        "session_id": 1,
        "run_id": 1,
        "contract": p::CONTEXT,
        "payload": "x",
        "execution": {
            "owner": { "id": "child", "generation": 999 },
            "scope": "a",
            "call": 999,
            "next": null,
            "job": null,
        },
    }))
    .unwrap();
    assert_eq!(
        captured
            .call(injected, Cancellation::default())
            .await
            .into_result()
            .unwrap(),
        "tail(x)"
    );
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn finalizer_can_use_declared_dependency_after_its_own_admission_closes() {
    let observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = observed.clone();
    let owner = Package::new("owner").service(p::INSTANCE_STOP, move |_: (), cx| {
        let flag = flag.clone();
        async move {
            let value: String = cx.call(p::CONTEXT, &"cleanup").await?;
            assert_eq!(value, "tail(cleanup)");
            flag.store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        }
    });
    let kernel = mount(
        vec![tail(), owner],
        json!({ "instances": [{ "id": "owner", "package": "owner", "dependencies": ["tail"] }] }),
    )
    .await;
    kernel.shutdown().await.unwrap();
    assert!(observed.load(std::sync::atomic::Ordering::Acquire));
}

#[tokio::test]
async fn explicitly_scoped_instance_cannot_reach_a_sibling_through_root_binding() {
    let scoped = Package::new("scoped").service("test.scoped", |_: (), cx| async move {
        assert_eq!(cx.identity().unwrap().scope, "a");
        assert_eq!(
            cx.call_in::<_, String>("b", p::CONTEXT, &"x")
                .await
                .unwrap_err()
                .code,
            "InvalidScope"
        );
        cx.call::<_, String>(p::CONTEXT, &"x").await
    });
    let kernel = mount(
        vec![tail(), scoped],
        json!({
            "instances": [{ "id": "scoped", "package": "scoped", "scope": "a" }],
            "scopes": {
                "": { "bindings": { "test.scoped": { "tail": "scoped" } } },
                "a": { "parent": "" },
                "b": { "parent": "" },
            },
        }),
    )
    .await;
    let value = kernel
        .invoke(
            serde_json::from_value(json!({
                "session_id": 1,
                "run_id": 1,
                "contract": "test.scoped",
                "payload": null,
            }))
            .unwrap(),
            Cancellation::default(),
        )
        .await
        .into_result()
        .unwrap();
    assert_eq!(value, "tail(x)");
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn lr_preview_keeps_scope_override_isolated_and_includes_owned_children() {
    let child = Package::new("child").service(p::CONTEXT, |v: String, _| async move { Ok(v) });
    let owned = Package::new("owned");
    let sibling = Package::new("sibling");
    let kernel = mount(
        vec![tail(), child, owned, sibling],
        json!({
            "instances": [
                { "id": "child", "package": "child", "scope": "a" },
                { "id": "owned", "package": "owned", "owner": "child" },
                { "id": "sibling", "package": "sibling", "scope": "b" }
            ],
            "scopes": {
                "a": { "parent": "", "bindings": { (p::CONTEXT): { "tail": "child" } } },
                "b": { "parent": "" },
            },
        }),
    )
    .await;
    let mut candidate = kernel.composition().clone();
    candidate.runtime.instances[0].config = Some(json!({ "changed": true }));
    assert_eq!(
        kernel.affected_instances(&candidate).unwrap(),
        vec!["child", "owned"]
    );
    assert!(
        !kernel
            .affected_by_contract(p::CONTEXT, &["child".into()])
            .unwrap()
    );
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn lr_management_gate_retains_unrelated_calls_and_rejects_retained_handle() {
    let child = Package::new("child").service("child", |v: String, _| async move { Ok(v) });
    let kernel = mount(
        vec![tail(), child],
        json!({ "scopes": { "": { "bindings": { "child": { "tail": "child" } } } } }),
    )
    .await;
    let handle = kernel.role(p::CONTEXT).unwrap();
    kernel.set_reconfiguring(&["tail".into()], true).unwrap();
    let request = Request {
        execution: None,
        session_id: 1,
        run_id: 1,
        contract: p::CONTEXT.into(),
        payload: json!("x"),
    };
    assert_eq!(
        handle
            .call(request.clone(), Cancellation::default())
            .await
            .into_result()
            .unwrap_err()
            .code,
        "Reconfiguring"
    );
    assert_eq!(
        kernel
            .invoke(request, Cancellation::default())
            .await
            .into_result()
            .unwrap_err()
            .code,
        "Reconfiguring"
    );
    let request = Request {
        execution: None,
        session_id: 1,
        run_id: 1,
        contract: "child".into(),
        payload: json!("ok"),
    };
    assert_eq!(
        kernel
            .invoke(request, Cancellation::default())
            .await
            .into_result()
            .unwrap(),
        json!("ok")
    );
    kernel.set_reconfiguring(&["tail".into()], false).unwrap();
    assert_eq!(invoke(&kernel, "x").await, "tail(x)");
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn auxiliary_author_cannot_reenter_foreground_and_wait_on_itself() {
    let package = tail()
        .service(p::coding::PROVIDER, |_: Value, _| async { Ok(Value::Null) })
        .service(
            p::auxiliary::PROVIDER,
            |_: p::auxiliary::Request, cx| async move {
                cx.call::<_, Value>(p::coding::PROVIDER, &Value::Null).await
            },
        );
    let kernel = mount(
        vec![package],
        json!({
            "scopes": {
                "": {
                    "bindings": {
                        (p::coding::PROVIDER): { "tail": "tail" },
                        (p::auxiliary::PROVIDER): { "tail": "tail" },
                    },
                },
            },
        }),
    )
    .await;
    let request = Request {
        execution: None,
        session_id: 1,
        run_id: 0,
        contract: p::auxiliary::PROVIDER.into(),
        payload: json!({
            "op": "generate",
            "purpose": "test",
            "timeout_ms": 1000,
            "input": { "target": null, "items": [], "tools": [], "max_output_tokens": 1 },
        }),
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        kernel.invoke(request, Cancellation::default()),
    )
    .await
    .unwrap();
    assert_eq!(result.into_result().unwrap_err().code, "InvalidInput");
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn provider_preparation_keeps_admitted_revision_after_new_input() {
    let events = Events::new(1);
    let entered = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let resume = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let provider_entered = entered.clone();
    let provider_resume = resume.clone();
    let package = tail().service(p::coding::PROVIDER, move |_: Value, cx| {
        let entered = provider_entered.clone();
        let resume = provider_resume.clone();
        async move {
            entered.add_permits(1);
            resume.acquire().await.unwrap().forget();
            let admitted: u64 = cx
                .call(
                    p::runtime::HOST,
                    &p::runtime::HostRequest::InvocationSnapshotRevision,
                )
                .await?;
            let current = cx.snapshot_revision().await?;
            Ok::<_, p::Fault>(json!({ "admitted": admitted, "current": current }))
        }
    });
    let kernel = std::sync::Arc::new(
        mount_events(
            vec![package],
            json!({
                "scopes": { "": { "bindings": { (p::coding::PROVIDER): { "tail": "tail" } } } },
            }),
            events.clone(),
        )
        .await,
    );
    let worker_kernel = kernel.clone();
    let worker = tokio::spawn(async move {
        worker_kernel
            .invoke(
                Request {
                    execution: None,
                    session_id: 1,
                    run_id: 1,
                    contract: p::coding::PROVIDER.into(),
                    payload: Value::Null,
                },
                Cancellation::default(),
            )
            .await
    });
    entered.acquire().await.unwrap().forget();
    events.invalidate_snapshots();
    resume.add_permits(1);
    let result = worker.await.unwrap().into_result().unwrap();
    assert!(result["admitted"].as_u64().unwrap() < result["current"].as_u64().unwrap());
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn auxiliary_consumers_receive_cleanup_failure_separately_from_the_reply() {
    let package = tail()
        .service(
            p::auxiliary::PROVIDER,
            |_: p::auxiliary::Request, cx| async move {
                cx.scope.cleanup(async {
                    Err(p::Fault::new(
                        "CustomCleanup",
                        "provider",
                        "socket cleanup failed",
                    ))
                })?;
                Ok::<_, p::Fault>(json!({ "items": [], "usage": { "tokens": 1 } }))
            },
        )
        .service("test.auxiliary-consumer", |_: (), cx| async move {
            let terminal = cx
                .call_terminal(
                    p::auxiliary::PROVIDER,
                    &json!({
                        "op": "generate",
                        "purpose": "notes",
                        "timeout_ms": 1000,
                        "input": {
                            "target": null,
                            "items": [],
                            "tools": [],
                            "max_output_tokens": 1,
                        },
                    }),
                )
                .await?;
            Ok::<_, p::Fault>(json!(terminal))
        });
    let kernel = mount(
        vec![package],
        json!({
            "scopes": {
                "": {
                    "bindings": {
                        (p::auxiliary::PROVIDER): { "tail": "tail" },
                        "test.auxiliary-consumer": { "tail": "tail" },
                    },
                },
            },
        }),
    )
    .await;
    let value = kernel
        .invoke(
            Request {
                execution: None,
                session_id: 1,
                run_id: 0,
                contract: "test.auxiliary-consumer".into(),
                payload: Value::Null,
            },
            Cancellation::default(),
        )
        .await
        .into_result()
        .unwrap();
    assert_eq!(value["cleanup_errors"][0]["code"], "CleanupFailure");
    assert!(
        value["cleanup_errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("socket cleanup failed")
    );
    assert_eq!(value["outcome"]["status"], "completed");
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_auxiliary_timeout_waits_for_the_author_cleanup_barrier() {
    let cleanup_entered = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let entered = cleanup_entered.clone();
    let provider_release = release.clone();
    let package = tail().service(
        p::auxiliary::PROVIDER,
        move |_: p::auxiliary::Request, cx| {
            let entered = entered.clone();
            let release = provider_release.clone();
            async move {
                cx.scope.cleanup(async move {
                    entered.add_permits(1);
                    release.acquire().await.unwrap().forget();
                    Ok(())
                })?;
                cx.scope.cancellation().cancelled().await;
                Ok::<_, p::Fault>(Value::Null)
            }
        },
    );
    let kernel = std::sync::Arc::new(
        mount(
            vec![package],
            json!({
                "scopes": { "": { "bindings": { (p::auxiliary::PROVIDER): { "tail": "tail" } } } },
            }),
        )
        .await,
    );
    let worker_kernel = kernel.clone();
    let worker = tokio::spawn(async move {
        worker_kernel
            .invoke(
                Request {
                    execution: None,
                    session_id: 1,
                    run_id: 0,
                    contract: p::auxiliary::PROVIDER.into(),
                    payload: json!({
                        "op": "generate",
                        "purpose": "test",
                        "timeout_ms": 10,
                        "input": {
                            "target": null,
                            "items": [],
                            "tools": [],
                            "max_output_tokens": 1,
                        },
                    }),
                },
                Cancellation::default(),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), cleanup_entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert!(!worker.is_finished());
    release.add_permits(1);
    assert_eq!(
        worker.await.unwrap().into_result().unwrap_err().code,
        "Timeout"
    );
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_before_dispatch_reports_cancellation_without_entering_service() {
    let entered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observer = entered.clone();
    let package =
        Package::new("cancel-probe").service("test.cancel-before-admission", move |_: (), _| {
            observer.store(true, std::sync::atomic::Ordering::SeqCst);
            async { Ok::<_, p::Fault>(()) }
        });
    let kernel = mount(
        vec![tail(), package],
        json!({
            "scopes": {
                "": { "bindings": { "test.cancel-before-admission": { "tail": "cancel-probe" } } },
            },
        }),
    )
    .await;
    let cancel = Cancellation::default();
    cancel.cancel();
    let terminal = kernel
        .invoke(
            Request {
                execution: None,
                session_id: 1,
                run_id: 1,
                contract: "test.cancel-before-admission".into(),
                payload: Value::Null,
            },
            cancel,
        )
        .await;
    assert!(
        matches!(terminal.outcome, p::Outcome::Cancelled),
        "{:?}",
        terminal.outcome
    );
    assert!(!entered.load(std::sync::atomic::Ordering::SeqCst));
    assert!(terminal.cleanup_errors.is_empty());
    kernel.shutdown().await.unwrap();
}
