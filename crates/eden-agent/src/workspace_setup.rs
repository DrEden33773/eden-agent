//! Workspace resolution for a session: validation, package references and the initial snapshot.
use super::*;
pub(crate) fn prepare(
    composition: &Path,
    cwd: &str,
    workspace_options: &WorkspaceOptions,
    events: &Arc<Events>,
    history: Option<&Path>,
) -> Result<eden_protocol::Composition, Fault> {
    let workspace = eden_workspace::Workspace::discover(Path::new(cwd), workspace_options)?;
    for diagnostic in &workspace.diagnostics {
        events.push(
            0,
            "resource_diagnostic",
            serde_json::json!({ "level": diagnostic.level, "message": diagnostic.message }),
        );
    }
    let mut selected: eden_protocol::Composition = serde_json::from_slice(
        &std::fs::read(composition)
            .map_err(|e| Fault::new("FileFailure", "composition", e.to_string()))?,
    )
    .map_err(|e| Fault::new("InvalidInput", "composition", e.to_string()))?;
    for package in &mut selected.packages {
        if let Some(config) = workspace
            .settings
            .get("plugins")
            .and_then(|plugins| plugins.get(&package.descriptor.package))
        {
            eden_workspace::merge(&mut package.config, config.clone());
        }
        if package
            .descriptor
            .provides
            .iter()
            .any(|role| role == eden_protocol::resources::SOURCE)
        {
            let mut config = package.config.clone();
            if !config.is_object() {
                config = serde_json::json!({});
            }
            config["cwd"] = serde_json::json!(cwd);
            config["global_dir"] = serde_json::json!(workspace.global_dir);
            config["trusted"] = serde_json::json!(workspace.trusted);
            config["settings"] = workspace.settings.clone();
            package.config = config;
        }
        if package.descriptor.package == "search" {
            if !package.config.is_object() {
                package.config = serde_json::json!({});
            }
            package.config["history_dir"] =
                serde_json::json!(workspace.global_dir.join("search-history"));
            let mut excluded = package.config["excluded_paths"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            excluded.push(serde_json::json!(workspace.global_dir));
            excluded.push(serde_json::json!(Path::new(cwd).join(".eden/sessions")));
            if let Some(history) = history {
                let history = if history.is_absolute() {
                    history.to_owned()
                } else {
                    std::env::current_dir()
                        .map_err(|e| Fault::new("FileFailure", "history", e.to_string()))?
                        .join(history)
                };
                excluded.push(serde_json::json!(history));
            }
            package.config["excluded_paths"] = serde_json::json!(excluded);
        }
        if package.descriptor.package == "distribution" {
            if !package.config.is_object() {
                package.config = serde_json::json!({});
            }
            package.config["root"] = serde_json::json!(workspace.global_dir.join("distribution"));
        }
        if package.descriptor.package == "coding-tools" {
            if !package.config.is_object() {
                package.config = serde_json::json!({});
            }
            for key in ["tools", "exclude_tools", "read_only"] {
                if let Some(value) = workspace.settings.get(key) {
                    package.config[key] = value.clone();
                }
            }
        }
    }
    eden_kernel::preflight(&selected)?;
    eden_workspace::packages::resolve_paths(
        &mut selected,
        composition.parent().unwrap_or(Path::new(".")),
        &workspace.global_dir.join("distribution"),
    )?;
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
