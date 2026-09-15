//! Explicitly ordered hooks and conflict-checked command contributions.
use eden_plugin_sdk::{
    CallContext, Package,
    protocol::{Descriptor, Fault, coding::ToolRequest, resources::*},
    serde_json::{self, Value, json},
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
const CATALOG: &str = "eden.command-catalog.v1";
#[derive(Clone, Default, Deserialize)]
struct Config {
    #[serde(default)]
    commands: Vec<Contribution>,
    #[serde(default)]
    input_hooks: Vec<String>,
    #[serde(default)]
    tool_hooks: Vec<String>,
}
#[derive(Clone, Deserialize)]
struct Contribution {
    catalog: String,
    execute: String,
}
fn invalid(message: impl Into<String>) -> Fault {
    Fault::new("InvalidInput", "contributions", message)
}
async fn commands(
    config: &Config,
    cx: &CallContext,
    cwd: &str,
) -> Result<BTreeMap<String, (CommandDefinition, String)>, Fault> {
    let mut all = BTreeMap::new();
    for source in &config.commands {
        let catalog: CommandCatalog = cx
            .call(&source.catalog, &CatalogRequest { cwd: cwd.into() })
            .await?;
        for command in catalog.commands {
            let name = command.name.clone();
            if all
                .insert(name.clone(), (command, source.execute.clone()))
                .is_some()
            {
                return Err(invalid(format!("duplicate command contribution: {name}")));
            }
        }
    }
    Ok(all)
}
fn descriptor() -> Descriptor {
    Descriptor {
        package: "contributions".into(),
        version: "0.1.0".into(),
        provides: vec![
            CATALOG.into(),
            COMMAND.into(),
            BEFORE_INPUT.into(),
            BEFORE_TOOL.into(),
        ],
    }
}
fn create(value: Value) -> Result<Package, Fault> {
    let config: Config = serde_json::from_value(if value.is_null() { json!({}) } else { value })
        .map_err(|e| invalid(e.to_string()))?;
    for hooks in [&config.input_hooks, &config.tool_hooks] {
        let mut seen = BTreeSet::new();
        for hook in hooks {
            if hook.is_empty()
                || !seen.insert(hook)
                || [BEFORE_INPUT, BEFORE_TOOL].contains(&hook.as_str())
            {
                return Err(invalid(
                    "hook order contains an empty, duplicate or recursive contract",
                ));
            }
        }
    }
    let catalogue = config.clone();
    let invocation = config.clone();
    let input = config.input_hooks;
    let tool = config.tool_hooks;
    Ok(Package::new("contributions")
        .service(CATALOG, move |request: CatalogRequest, cx| {
            let config = catalogue.clone();
            async move {
                Ok(CommandCatalog {
                    commands: commands(&config, &cx, &request.cwd)
                        .await?
                        .into_values()
                        .map(|(definition, _)| definition)
                        .collect(),
                })
            }
        })
        .service(COMMAND, move |request: CommandRequest, cx| {
            let config = invocation.clone();
            async move {
                let all = commands(&config, &cx, &request.cwd).await?;
                let (_, contract) = all
                    .get(&request.name)
                    .ok_or_else(|| invalid(format!("unknown command: {}", request.name)))?;
                cx.call::<_, Value>(contract, &request).await
            }
        })
        .service(BEFORE_INPUT, move |request: InputHook, cx| {
            let hooks = input.clone();
            async move {
                let revision = request.resource_revision;
                let mut current = request;
                for contract in hooks {
                    current = cx.call(&contract, &current).await?;
                    if current.resource_revision != revision {
                        return Err(invalid("input hook cannot change the resource revision"));
                    }
                }
                Ok(current)
            }
        })
        .service(BEFORE_TOOL, move |request: ToolRequest, cx| {
            let hooks = tool.clone();
            async move {
                let mut current = request.clone();
                for contract in hooks {
                    current = cx.call(&contract, &current).await?;
                    if current.call_id != request.call_id || current.cwd != request.cwd {
                        return Err(invalid("tool hook cannot change call identity or cwd"));
                    }
                }
                Ok(current)
            }
        }))
}
eden_plugin_sdk::export_plugin!(descriptor, create);
