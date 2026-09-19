//! An independent domain unknown to the host's Rust types and role enumeration.
use eden_plugin_sdk::{
    Package,
    protocol::{Descriptor, Fault, INSTANCE_STOP, resources::*},
    serde_json::{Value, json},
};
fn descriptor() -> Descriptor {
    Descriptor {
        package: "service-a".into(),
        version: "0.1.0".into(),
        provides: vec![
            "example.compute.v1".into(),
            SOURCE.into(),
            INSTANCE_STOP.into(),
        ],
    }
}
fn create(config: Value) -> Result<Package, Fault> {
    if config["fail_init"] == true {
        return Err(Fault::new(
            "Unavailable",
            "service-a",
            "deliberate initialization failure",
        ));
    }
    let marker = config["stop_marker"].as_str().map(str::to_owned);
    Ok(Package::new("service-a")
        .service("example.compute.v1", |request: Value, _| async move {
            let value = request["value"]
                .as_i64()
                .ok_or_else(|| Fault::new("InvalidInput", "service-a", "value must be integer"))?;
            Ok(json!({ "answer": value * 7, "author": "independent-a" }))
        })
        .service(SOURCE, |request: ResourceRequest, _| async move {
            Ok(ResourceReply {
                snapshot: Snapshot {
                    revision: 41,
                    instructions: "Independent resource source marker: external-a.".into(),
                    ..Default::default()
                },
                text: match request {
                    ResourceRequest::Expand { text } => Some(text),
                    _ => None,
                },
            })
        })
        .service(INSTANCE_STOP, move |_: Value, _| {
            let marker = marker.clone();
            async move {
                if let Some(path) = marker {
                    std::fs::write(path, b"service-a stopped")
                        .map_err(|e| Fault::new("CleanupFailure", "service-a", e.to_string()))?;
                }
                Ok(())
            }
        }))
}
eden_plugin_sdk::export_plugin!(descriptor, create);
