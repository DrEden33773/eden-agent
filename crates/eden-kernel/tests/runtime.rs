//! Public runtime behavior shared by local and native authors.
#![allow(missing_docs)]
use eden_kernel::{Events, Kernel};
use eden_plugin_sdk::{Cancellation, Package};
use eden_protocol::{self as p, Request};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

async fn mount(packages: Vec<Package>, graph: Value) -> Kernel {
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
    Kernel::load_embedded(composition, Path::new("."), 1, Events::new(1), local)
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
