//! A second independent native presentation owner for slot and action-routing acceptance.
use eden_plugin_sdk::{
    Package,
    protocol::{Descriptor, Fault, presentation::*},
    serde_json::{Value, json},
};
const PEER: &str = "eden.test.presentation-peer.v1";
fn descriptor() -> Descriptor {
    Descriptor {
        package: "presentation-peer".into(),
        version: "0.1.0".into(),
        provides: vec![PEER.into(), ACTION.into()],
    }
}
fn create(_: Value) -> Result<Package, Fault> {
    Ok(Package::new("presentation-peer")
        .service(PEER, |slot: Slot, cx| async move {
            cx.present(
                View::new("peer", slot, "Independent peer").node(Node::Button {
                    id: "peer-button".into(),
                    action: "peer-action".into(),
                    label: "Peer action".into(),
                }),
            )
            .await
        })
        .service(ACTION, |_: ActionRequest, _| async {
            Ok::<_, Fault>(json!({ "owner": "presentation-peer" }))
        }))
}
eden_plugin_sdk::export_plugin!(descriptor, create);
