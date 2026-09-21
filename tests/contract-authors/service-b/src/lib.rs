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
fn create(_: Value) -> Result<Package, Fault> {
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
            |request: CommandRequest, cx| async move {
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
