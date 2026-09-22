//! Independent extension reaches semantic host interactions through the public byte bridge.
use eden_plugin_sdk::{
    Package,
    protocol::{
        Descriptor, Fault,
        interaction::{HOST, Interaction},
        resources::*,
    },
    serde_json::{Value, json},
};
fn descriptor() -> Descriptor {
    Descriptor {
        package: "entrypoint-extension".into(),
        version: "0.1.0".into(),
        provides: vec![
            "example.entry-catalog.v1".into(),
            "example.entry-command.v1".into(),
            "example.entry-input.v1".into(),
        ],
    }
}
fn create(_: Value) -> Result<Package, Fault> {
    Ok(Package::new("entrypoint-extension")
        .service("example.entry-catalog.v1", |_: CatalogRequest, _| async {
            Ok(CommandCatalog {
                commands: vec![CommandDefinition {
                    name: "ask-host".into(),
                    description: "Ask the calling host a semantic question".into(),
                    parameters: json!({ "type": "object" }),
                }],
            })
        })
        .service(
            "example.entry-command.v1",
            |request: CommandRequest, cx| async move {
                cx.emit("external_command_entered", json!({ "name": request.name }))?;
                cx.call::<_, Value>(
                    HOST,
                    &Interaction::Request {
                        kind: "confirm".into(),
                        title: "External extension question".into(),
                        options: vec![],
                        initial: String::new(),
                        timeout_ms: Some(10000),
                    },
                )
                .await
            },
        )
        .service(
            "example.entry-input.v1",
            |request: InputHook, cx| async move {
                cx.emit(
                    "external_input_hook",
                    json!({ "revision": request.resource_revision }),
                )?;
                Ok(request)
            },
        ))
}
eden_plugin_sdk::export_plugin!(descriptor, create);
