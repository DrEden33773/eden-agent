//! Runtime contracts exercised without an agent, kernel, or native plugin.
use eden_presentation::Hub;
use eden_protocol::{Fault, presentation as p};
use serde_json::json;
use std::sync::Arc;

fn fixture() -> (Arc<Hub>, p::ActionRequest) {
    let hub = Arc::new(Hub::default());
    hub.set_shared();
    hub.begin_run(7);
    let revision = hub
        .publish(
            "author",
            7,
            p::View::new("question", p::Slot::Panel, "Question").node(p::Node::Form {
                id: "form".into(),
                action: "answer".into(),
                fields: vec![p::Field::text("value", "Value", true)],
            }),
        )
        .unwrap();
    let request = p::ActionRequest {
        session_id: 11,
        owner: "author".into(),
        view_id: "question".into(),
        revision: revision.value,
        action: "answer".into(),
        request_id: "request-1".into(),
        values: json!({ "value": "yes" }),
    };
    (hub, request)
}

#[tokio::test]
async fn retries_share_one_dispatch_and_keep_the_result_after_detach() {
    let (hub, request) = fixture();
    let attachment = hub.attach("web").unwrap();
    let mut first = hub.admit_action(11, &request, || Ok("generation")).unwrap();
    assert_eq!(first.take_dispatch(), Some((7, "generation")));
    let retry = hub
        .admit_action(11, &request, || -> Result<(), Fault> {
            panic!("retry must not resolve again")
        })
        .unwrap();
    hub.detach(attachment).unwrap();
    hub.complete_action(request.clone(), Ok(json!("accepted")));
    assert_eq!(first.result().await.unwrap(), json!("accepted"));
    assert_eq!(retry.result().await.unwrap(), json!("accepted"));
    let cached = hub
        .admit_action(11, &request, || -> Result<(), Fault> {
            panic!("cached retry must not resolve again")
        })
        .unwrap();
    assert_eq!(cached.result().await.unwrap(), json!("accepted"));
}

#[tokio::test]
async fn drain_rejects_new_actions_but_waits_for_admitted_cleanup() {
    let (hub, request) = fixture();
    let admitted = hub.admit_action(11, &request, || Ok(())).unwrap();
    hub.begin_drain(7);
    let mut next = request.clone();
    next.request_id = "request-2".into();
    assert_eq!(
        hub.admit_action(11, &next, || Ok(())).err().unwrap().code,
        "Unavailable"
    );
    let waiting = hub.wait_for_actions(7);
    tokio::pin!(waiting);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(waiting.as_mut().poll(cx).is_pending()))
            .await
    );
    hub.complete_action(request, Ok(json!(true)));
    waiting.await;
    assert_eq!(admitted.result().await.unwrap(), json!(true));
    hub.end_run(7);
    assert!(!hub.snapshot(11).views[0].active);
}

#[tokio::test]
async fn failed_resolution_does_not_claim_a_form_and_failed_execution_releases_it() {
    let (hub, request) = fixture();
    assert!(
        hub.admit_action(11, &request, || -> Result<(), Fault> {
            Err(Fault::new("Unavailable", "test", "missing generation"))
        })
        .is_err()
    );
    assert!(hub.snapshot(11).views[0].handled_actions.is_empty());
    let failed = hub.admit_action(11, &request, || Ok(())).unwrap();
    hub.complete_action(
        request.clone(),
        Err(Fault::new("Unavailable", "test", "failed")),
    );
    assert!(failed.result().await.is_err());
    let mut next = request;
    next.request_id = "request-2".into();
    let mut retried = hub.admit_action(11, &next, || Ok(())).unwrap();
    assert!(retried.take_dispatch().is_some());
}

#[tokio::test]
async fn management_forms_survive_run_drain_and_allow_repeated_actions() {
    let hub = Arc::new(Hub::default());
    hub.set_shared();
    hub.begin_run(7);
    let binding = eden_protocol::configuration_form::Binding {
        instance: "notes".into(),
        generation: Some(2),
        revision: 3,
        profile: 0,
    };
    let revision = hub
        .publish_management(
            "eden-host-configuration",
            p::View::new("settings", p::Slot::Panel, "Settings").node(p::Node::ConfigurationForm {
                id: "config".into(),
                binding: binding.clone(),
                fields: vec![],
            }),
        )
        .unwrap();
    hub.begin_drain(7);
    hub.end_run(7);
    let request = p::ActionRequest {
        session_id: 11,
        owner: "eden-host-configuration".into(),
        view_id: "settings".into(),
        revision: revision.value,
        action: "config:validate".into(),
        request_id: "first".into(),
        values: json!({ "binding": binding, "edits": [] }),
    };
    let first = hub.admit_action(11, &request, || Ok(())).unwrap();
    hub.complete_action(request.clone(), Ok(json!(true)));
    assert_eq!(first.result().await.unwrap(), json!(true));
    let mut next = request;
    next.request_id = "second".into();
    assert!(
        hub.admit_action(11, &next, || Ok(()))
            .unwrap()
            .take_dispatch()
            .is_some()
    );
    assert!(hub.snapshot(11).views[0].active);
    assert!(hub.snapshot(11).views[0].handled_actions.is_empty());
    assert!(hub.static_record(0, &[]).views.is_empty());
    hub.invalidate_instances(&["notes".into()]);
    assert!(hub.snapshot(11).views[0].active);
    hub.begin_drain(0);
    hub.wait_for_actions(0).await;
    hub.end_run(0);
    assert!(hub.snapshot(11).views[0].active);
}

#[tokio::test]
async fn configuration_admission_rejects_unknown_secret_readonly_and_duplicate_edits() {
    use eden_protocol::configuration_form::{Binding, Control, Field};
    let hub = Arc::new(Hub::default());
    hub.set_shared();
    let binding = Binding {
        instance: "notes".into(),
        generation: Some(1),
        revision: 1,
        profile: 0,
    };
    let field = |path: &str, control, writable| Field {
        path: path.into(),
        label: path.into(),
        description: None,
        control,
        value: None,
        options: vec![],
        item_kind: None,
        source: None,
        writable,
        configured: false,
    };
    let revision = hub
        .publish_management(
            "eden-host-configuration",
            p::View::new("settings", p::Slot::Panel, "Settings").node(p::Node::ConfigurationForm {
                id: "config".into(),
                binding: binding.clone(),
                fields: vec![
                    field("/public", Control::Text, true),
                    field("/secret", Control::Secret, true),
                    field("/readonly", Control::Text, false),
                ],
            }),
        )
        .unwrap();
    for edits in [
        json!([{ "operation": "set", "path": "/unknown", "value": "x" }]),
        json!([{ "operation": "set", "path": "/secret", "value": "x" }]),
        json!([{ "operation": "clear", "path": "/readonly" }]),
        json!([
            { "operation": "clear", "path": "/public" },
            { "operation": "inherit", "path": "/public" }
        ]),
    ] {
        let request = p::ActionRequest {
            session_id: 11,
            owner: "eden-host-configuration".into(),
            view_id: "settings".into(),
            revision: revision.value,
            action: "config:apply".into(),
            request_id: "attempt".into(),
            values: json!({ "binding": binding, "edits": edits }),
        };
        assert_eq!(
            hub.admit_action(11, &request, || Ok(()))
                .err()
                .unwrap()
                .code,
            "InvalidInput"
        );
    }
}

#[test]
fn static_projection_uses_the_latest_matching_source_and_excludes_live_controls() {
    let hub = Hub::default();
    hub.begin_run(7);
    hub.publish(
        "author",
        7,
        p::View::new("result", p::Slot::Panel, "Result").source(p::Source {
            record_sequence: None,
            class: p::ContentClass::Tool,
        }),
    )
    .unwrap();
    let records = ["tool_result", "terminal"]
        .into_iter()
        .enumerate()
        .map(|(index, kind)| {
            serde_json::from_value(json!({
                "schema_version": 1,
                "session_id": 11,
                "sequence": index + 1,
                "run_id": 7,
                "kind": kind,
                "payload": {},
            }))
            .unwrap()
        })
        .collect::<Vec<eden_protocol::coding::Record>>();
    let saved = hub.static_record(7, &records);
    let view: p::LiveView = serde_json::from_value(saved.views[0].clone()).unwrap();
    assert!(!view.active);
    assert_eq!(view.view.source.unwrap().record_sequence, Some(1));
}
