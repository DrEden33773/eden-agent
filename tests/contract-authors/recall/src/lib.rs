//! Independent native consumer of bounded recall using only published SDK payloads.
use eden_plugin_sdk::{
    Package,
    protocol::{
        Descriptor, Fault,
        recall::{RECALL, RecallReply, RecallRequest},
    },
    serde_json::Value,
};
const CONTRACT: &str = "test.recall-author.v1";
fn descriptor() -> Descriptor {
    Descriptor {
        package: "recall-author".into(),
        version: "0.1.0".into(),
        provides: vec![CONTRACT.into()],
    }
}
fn create(_: Value) -> Result<Package, Fault> {
    Ok(
        Package::new("recall-author").service(CONTRACT, |request: RecallRequest, cx| async move {
            cx.call::<_, RecallReply>(RECALL, &request).await
        }),
    )
}
eden_plugin_sdk::export_plugin!(descriptor, create);
