//! A separately built client of A, plus command and before-hook contributions.
use eden_plugin_sdk::{
    Package,
    protocol::{
        Descriptor, Fault,
        coding::{Block, ToolRequest},
        resources::*,
    },
    serde_json::{Value, json},
};
fn descriptor() -> Descriptor {
    Descriptor {
        package: "service-b".into(),
        version: "0.1.0".into(),
        provides: vec![
            "example.client.v1".into(),
            "example.commands.v1".into(),
            "example.command.v1".into(),
            "example.input.v1".into(),
            "example.tool.v1".into(),
        ],
    }
}
fn create(config: Value) -> Result<Package, Fault> {
    let intent_probe = config["intent_probe"] == true;
    Ok(Package::new("service-b")
        .service("example.client.v1", |request: Value, cx| async move {
            let reply: Value = cx.call("example.compute.v1", &request).await?;
            Ok(json!({ "via": "independent-b", "result": reply }))
        })
        .service("example.commands.v1", |_: CatalogRequest, _| async {
            Ok(CommandCatalog {
                commands: vec![CommandDefinition {
                    name: "example.compute".into(),
                    description: "Independent B calls independent A through an author-defined \
                                  contract."
                        .into(),
                    parameters: json!({
                        "type": "object",
                        "properties": { "value": { "type": "integer" } },
                        "required": ["value"],
                    }),
                }],
            })
        })
        .service(
            "example.command.v1",
            move |request: CommandRequest, cx| async move {
                if intent_probe {
                    let read = || -> Result<Value, Box<dyn std::error::Error>> {
                        let history = request.arguments["history"]
                            .as_str()
                            .ok_or("history missing")?;
                        let receipt = request.arguments["receipt"]
                            .as_str()
                            .ok_or("receipt missing")?;
                        let durable = std::fs::read(history)?;
                        // This write is the command's external effect; its contents were
                        // read from disk at entry, before invoking any downstream service.
                        std::fs::write(receipt, durable)?;
                        Ok(json!({ "observed": true }))
                    };
                    return read().map_err(|error| {
                        Fault::new("ProbeFailure", "service-b", error.to_string())
                    });
                }
                cx.call::<_, Value>("example.compute.v1", &request.arguments)
                    .await
            },
        )
        .service("example.input.v1", |mut request: InputHook, _| async move {
            request.content.push(Block::Text {
                text: "external-input-hook".into(),
            });
            Ok(request)
        })
        .service(
            "example.tool.v1",
            |mut request: ToolRequest, _| async move {
                if request.name == "write" {
                    request.arguments["content"] = json!("external-tool-hook\n");
                }
                Ok(request)
            },
        ))
}
eden_plugin_sdk::export_plugin!(descriptor, create);
