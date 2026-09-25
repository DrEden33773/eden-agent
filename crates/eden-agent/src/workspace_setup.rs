//! Workspace resolution for a session: discovery, the settings handed to each
//! package, composition preflight, path resolution and the package references
//! the session records.
use super::*;
pub(crate) fn prepare(
    composition: &Path,
    cwd: &str,
    workspace_options: &WorkspaceOptions,
    events: &Arc<Events>,
    history: Option<&Path>,
) -> Result<eden_protocol::Composition, Fault> {
    let selected: eden_protocol::Composition = serde_json::from_slice(
        &std::fs::read(composition)
            .map_err(|e| Fault::new("FileFailure", "composition", e.to_string()))?,
    )
    .map_err(|e| Fault::new("InvalidInput", "composition", e.to_string()))?;
    prepare_resolved(
        selected,
        composition.parent().unwrap_or(Path::new(".")),
        cwd,
        workspace_options,
        events,
        history,
        &std::collections::BTreeSet::new(),
    )
}
pub(crate) fn prepare_resolved(
    mut selected: eden_protocol::Composition,
    base: &Path,
    cwd: &str,
    workspace_options: &WorkspaceOptions,
    events: &Arc<Events>,
    history: Option<&Path>,
    local: &std::collections::BTreeSet<String>,
) -> Result<eden_protocol::Composition, Fault> {
    let workspace = eden_workspace::Workspace::discover(Path::new(cwd), workspace_options)?;
    for diagnostic in &workspace.diagnostics {
        events.push(
            0,
            "resource_diagnostic",
            serde_json::json!({ "level": diagnostic.level, "message": diagnostic.message }),
        );
    }
    for package in &mut selected.packages {
        if let Some(config) = workspace
            .settings
            .get("plugins")
            .and_then(|plugins| plugins.get(&package.descriptor.package))
        {
            eden_workspace::merge(&mut package.config, config.clone());
        }
    }
    eden_kernel::preflight(&selected)?;
    let embedded: Vec<_> = selected
        .packages
        .iter()
        .filter(|p| local.contains(&p.descriptor.package))
        .cloned()
        .collect();
    selected
        .packages
        .retain(|p| !local.contains(&p.descriptor.package));
    eden_workspace::packages::resolve_paths(
        &mut selected,
        base,
        &workspace.global_dir.join("distribution"),
    )?;
    selected.packages.extend(embedded);
    let history_path = history
        .map(|path| {
            if path.is_absolute() {
                Ok(path.to_owned())
            } else {
                std::env::current_dir()
                    .map(|cwd| cwd.join(path))
                    .map_err(|error| Fault::new("FileFailure", "history", error.to_string()))
            }
        })
        .transpose()?;
    let environment = eden_protocol::environment::HostEnvironment {
        cwd: workspace.cwd,
        global_dir: workspace.global_dir,
        project_trusted: workspace.trusted,
        // Discovery has excluded untrusted project configuration.
        commands_trusted: true,
        settings: workspace.settings,
        history_path,
        managed_root: std::env::var_os("EDEN_MANAGED_ROOT").map(std::path::PathBuf::from),
        resource_packages: selected.resource_packages.clone(),
    };
    selected.host_environment = Some(environment.clone());
    let environment = serde_json::to_value(environment)
        .map_err(|error| Fault::new("InvalidInput", "host-environment", error.to_string()))?;
    for package in &mut selected.packages {
        if package.config.is_null() {
            package.config = serde_json::json!({});
        }
        if package.config.is_object() {
            package.config[eden_protocol::environment::CONFIG_KEY] = environment.clone();
        }
    }
    Ok(selected)
}

pub(crate) fn register(
    session: &Session,
    composition: &eden_protocol::Composition,
    pending: bool,
) -> Result<(), Fault> {
    if let Some(history) = &session.0.history_path {
        let workspace = eden_workspace::Workspace::discover(
            Path::new(session.cwd()),
            &session.0.workspace_options,
        )?;
        eden_workspace::packages::register_pending(
            &workspace.global_dir.join("distribution"),
            history,
            composition,
            pending,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod environment_tests {
    use super::*;

    #[test]
    fn every_author_receives_authoritative_host_environment() {
        let root = std::env::temp_dir().join(format!(
            "eden-host-env-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let global = root.join("global");
        std::fs::create_dir_all(&global).unwrap();
        let names = [
            "third-party",
            "model-access",
            "search",
            "distribution",
            "coding-tools",
        ];
        let mut packages: Vec<_> = names
            .iter()
            .map(|name| {
                serde_json::json!({
                    "descriptor": {
                        "package": name,
                        "version": "0.1.0",
                        "provides": [eden_protocol::delivery::EXPORTER],
                    },
                    "host": eden_protocol::CONTRACT,
                    "sdk": eden_protocol::CONTRACT,
                    "target": eden_plugin_sdk::abi::TARGET,
                    "library": "unused",
                    "config": { "__eden_host": { "cwd": "forged" }, "custom": 42 },
                })
            })
            .collect();
        let mut scalar = packages[0].clone();
        scalar["descriptor"]["package"] = serde_json::json!("scalar-author");
        scalar["config"] = serde_json::json!([1, 2, 3]);
        packages.push(scalar);
        let selected = serde_json::from_value(serde_json::json!({
            "packages": packages,
            "roles": { eden_protocol::delivery::EXPORTER: "third-party" },
        }))
        .unwrap();
        let result = prepare_resolved(
            selected,
            &root,
            root.to_str().unwrap(),
            &WorkspaceOptions {
                global_dir: global.clone(),
                project_trust: Some(false),
                overrides: serde_json::json!({
                    "plugins": { "third-party": { "__eden_host": { "cwd": "settings-forged" } } },
                }),
            },
            &Events::new(0),
            Some(&root.join("history.jsonl")),
            &names
                .into_iter()
                .chain(["scalar-author"])
                .map(String::from)
                .collect(),
        )
        .unwrap();
        assert_eq!(result.host_environment.as_ref().unwrap().cwd, root);
        for package in result.packages {
            if package.descriptor.package == "scalar-author" {
                assert_eq!(package.config, serde_json::json!([1, 2, 3]));
                continue;
            }
            assert_eq!(
                package.config["__eden_host"]["cwd"],
                serde_json::json!(root)
            );
            assert_eq!(
                package.config["__eden_host"]["global_dir"],
                serde_json::json!(global)
            );
            assert_eq!(package.config["__eden_host"]["project_trusted"], false);
            assert_eq!(package.config["custom"], 42);
            let host = eden_protocol::environment::HostEnvironment::from_config(&package.config)
                .unwrap()
                .unwrap();
            assert!(host.commands_trusted);
            assert_eq!(host.history_path, Some(root.join("history.jsonl")));
            assert!(package.config.get("artifact_dir").is_none());
            assert!(package.config.get("root").is_none());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
