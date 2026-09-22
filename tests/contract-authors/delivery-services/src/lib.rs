//! SDK-only delivery roles, built after freezing the host installation.
use eden_plugin_sdk::{
    Package,
    protocol::{Descriptor, Fault, delivery::*, updates::*},
    serde_json::{Value, json},
};
fn descriptor() -> Descriptor {
    Descriptor {
        package: "delivery-services".into(),
        version: "0.1.0".into(),
        provides: vec![EXPORTER.into(), SHARE_TARGET.into(), UPDATE_SOURCE.into()],
    }
}
fn create(_: Value) -> Result<Package, Fault> {
    Ok(Package::new("delivery-services")
        .service(EXPORTER, |request: ExportRequest, _| async move {
            Ok(Artifact {
                filename: "conversation.html".into(),
                media_type: "text/html".into(),
                content: format!("External renderer: {} records", request.records.len()),
                warnings: vec![],
            })
        })
        .service(SHARE_TARGET, |request: PublishRequest, _| async move {
            if !request.confirmed {
                return Err(Fault::new(
                    "ConfirmationRequired",
                    "author",
                    "confirm prepared bytes",
                ));
            }
            Ok(json!({
                "url": "https://example.invalid/author-target",
                "content": request.artifact.content,
                "visibility": "author-controlled",
            }))
        })
        .service(UPDATE_SOURCE, |_: UpdateRequest, _| async move {
            Ok(UpdateReply::Discovered {
                targets: vec![UpdateStatus {
                    target: UpdateTarget::Host,
                    current_version: Some("external-source".into()),
                    configured: false,
                    managed: false,
                    instructions: "External update source".into(),
                    candidate: None,
                }],
            })
        }))
}
eden_plugin_sdk::export_plugin!(descriptor, create);
