//! Explicit package preparation. Starting a session never downloads or builds.
mod files;
mod manager;
mod source;
use eden_plugin_sdk::{
    Package,
    protocol::{
        Descriptor, Fault,
        resources::{CatalogRequest, CommandCatalog, CommandDefinition, CommandRequest},
    },
    serde_json::{self, Value, json},
};
use std::{path::PathBuf, sync::Arc};
const CATALOG: &str = "eden.distribution-commands.v1";
const COMMAND: &str = "eden.distribution.v1";
fn descriptor() -> Descriptor {
    Descriptor {
        package: "distribution".into(),
        version: "0.1.0".into(),
        provides: vec![CATALOG.into(), COMMAND.into()],
    }
}
fn create(config: Value) -> Result<Package, Fault> {
    let root = config["root"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| manager::error("InvalidInput", "distribution.root is required"))?;
    if !root.is_absolute() {
        return Err(manager::error(
            "InvalidInput",
            "distribution.root must be absolute",
        ));
    }
    let manager = Arc::new(manager::Manager {
        root,
        client: source::client(config["ca_certificate"].as_str().map(std::path::Path::new))?,
    });
    Ok(Package::new("distribution")
        .service(CATALOG, |_: CatalogRequest, _| async {
            Ok(CommandCatalog {
                commands: catalog(),
            })
        })
        .service(COMMAND, move |request: CommandRequest, cx| {
            let manager = manager.clone();
            async move {
                let (sender, receiver) = eden_plugin_sdk::tokio::sync::oneshot::channel();
                let cancel = cx.scope.cancellation();
                cx.scope.spawn(async move {
                    let result = dispatch(&manager, request, cancel).await;
                    let _ = sender.send(result);
                    Ok(())
                })?;
                receiver
                    .await
                    .map_err(|_| manager::error("Unavailable", "package command completion lost"))?
            }
        }))
}
eden_plugin_sdk::export_plugin!(descriptor, create);
async fn dispatch(
    manager: &manager::Manager,
    request: CommandRequest,
    cancel: eden_plugin_sdk::Cancellation,
) -> Result<Value, Fault> {
    let args = &request.arguments;
    match request.name.as_str() {
        "package.install" => {
            let mut source: manager::Source = serde_json::from_value(args["source"].clone())
                .map_err(|e| manager::error("InvalidInput", e.to_string()))?;
            if let manager::Source::Local { path } = &mut source
                && !path.is_absolute()
            {
                *path = PathBuf::from(&request.cwd).join(&path);
            }
            manager.install(source, args["build"] == true, cancel).await
        }
        "package.list" => serde_json::to_value(manager.list()?)
            .map_err(|e| manager::error("InvalidInput", e.to_string())),
        "package.remove" => manager.remove(
            string(args, "name")?,
            string(args, "version")?,
            args["force"] == true,
        ),
        "package.resolve" => {
            let base_path = PathBuf::from(&request.cwd).join(string(args, "base")?);
            let mut base: eden_plugin_sdk::protocol::Composition =
                serde_json::from_slice(&std::fs::read(&base_path).map_err(manager::io)?)
                    .map_err(|e| manager::error("InvalidInput", e.to_string()))?;
            for package in &mut base.packages {
                package.library = base_path
                    .parent()
                    .ok_or_else(|| manager::error("InvalidInput", "composition has no parent"))?
                    .join(&package.library)
                    .to_string_lossy()
                    .into_owned();
            }
            manager.resolve(
                base,
                &args["packages"],
                args.get("roles").unwrap_or(&json!({})),
            )
        }
        _ => Err(manager::error("UnknownCommand", request.name)),
    }
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, Fault> {
    value[key]
        .as_str()
        .ok_or_else(|| manager::error("InvalidInput", format!("{key} must be a string")))
}
fn catalog() -> Vec<CommandDefinition> {
    [
        (
            "package.install",
            concat!(
                "Prepare a local directory/tar/tar.gz, pinned Git revision or HTTPS archive. Installation ",
                "does not enable the package.",
            ),
            json!({
                "source": { "type": "object" },
                "build": { "type": "boolean", "default": false },
            }),
            vec!["source"],
        ),
        (
            "package.list",
            "List installed versions and their locked source identities.",
            json!({}),
            vec![],
        ),
        (
            "package.remove",
            concat!(
                "Remove one installed version. Referenced versions require explicit force; history ",
                "is retained.",
            ),
            json!({
                "name": { "type": "string" },
                "version": { "type": "string" },
                "force": { "type": "boolean", "default": false },
            }),
            vec!["name", "version"],
        ),
        (
            "package.resolve",
            concat!(
                "Save an immutable resolved composition using installed versions. Missing dependencies ",
                "are never downloaded here.",
            ),
            json!({
                "base": { "type": "string" },
                "packages": { "type": "array" },
                "roles": { "type": "object" },
            }),
            vec!["base", "packages"],
        ),
    ]
    .into_iter()
    .map(
        |(name, description, properties, required)| CommandDefinition {
            name: name.into(),
            description: description.into(),
            parameters: json!({
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            }),
        },
    )
    .collect()
}
