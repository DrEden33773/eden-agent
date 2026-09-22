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
        provides: vec![
            EXPORTER.into(),
            SHARE_TARGET.into(),
            UPDATE_SOURCE.into(),
            eden_plugin_sdk::protocol::INSTANCE_STOP.into(),
        ],
    }
}
fn create(config: Value) -> Result<Package, Fault> {
    let fail = config["fail"].as_bool().unwrap_or(false);
    let bytes = config["bytes"].as_u64().unwrap_or(0) as usize;
    let marker = config["marker"].as_str().map(std::path::PathBuf::from);
    Ok(Package::new("delivery-services")
        .service(EXPORTER, move |request: ExportRequest, cx| {
            let marker = marker.clone();
            async move {
                if let Some(marker) = marker {
                    let cleaned = marker.with_extension("cleaned");
                    cx.scope
                        .cleanup(async move {
                            std::fs::write(cleaned, "done")
                                .map_err(|e| Fault::new("FileFailure", "author", e.to_string()))
                        })
                        .unwrap();
                    std::fs::write(marker, "ready").unwrap();
                    std::future::pending::<()>().await;
                }
                if fail {
                    return Err(Fault::new("ExportFailure", "author", "export failed"));
                }
                Ok(Artifact {
                    filename: "conversation.jsonl".into(),
                    media_type: "application/x-ndjson".into(),
                    content: if bytes > 0 {
                        "x".repeat(bytes)
                    } else {
                        let mut content =
                            String::from("{\"format\":\"eden-reading-v1\",\"restorable\":false}\n");
                        content.push_str(
                            &json!({
                                "message":
                                    format!("External renderer: {} records", request.records.len()),
                            })
                            .to_string(),
                        );
                        content.push('\n');
                        content
                    },
                    warnings: vec![],
                })
            }
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
        })
        .service(
            eden_plugin_sdk::protocol::INSTANCE_STOP,
            move |_: Value, _| async move {
                if fail {
                    Err(Fault::new("CleanupFailure", "author", "finalizer failed"))
                } else {
                    Ok(Value::Null)
                }
            },
        ))
}
eden_plugin_sdk::export_plugin!(descriptor, create);
