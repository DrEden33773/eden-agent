use super::*;
pub(crate) fn prepare(
    composition: &Path,
    cwd: &str,
    workspace_options: &WorkspaceOptions,
    events: &Arc<Events>,
) -> Result<eden_protocol::Composition, Fault> {
    let workspace = eden_workspace::Workspace::discover(Path::new(cwd), workspace_options)?;
    for diagnostic in &workspace.diagnostics {
        events.push(
            0,
            "resource_diagnostic",
            serde_json::json!({"message":diagnostic}),
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
    for package in &mut selected.packages {
        package.library = std::fs::canonicalize(
            composition
                .parent()
                .unwrap_or(Path::new("."))
                .join(&package.library),
        )
        .map_err(|e| Fault::new("FileFailure", "composition", e.to_string()))?
        .to_string_lossy()
        .into_owned();
    }
    eden_workspace::packages::validate(&selected, &workspace.global_dir.join("distribution"))?;
    Ok(selected)
}

pub(crate) fn register(
    session: &Session,
    composition: &eden_protocol::Composition,
) -> Result<(), Fault> {
    if let Some(history) = &session.0.history_path {
        let workspace = eden_workspace::Workspace::discover(
            Path::new(session.cwd()),
            &session.0.workspace_options,
        )?;
        eden_workspace::packages::register(
            &workspace.global_dir.join("distribution"),
            history,
            composition,
        )?;
    }
    Ok(())
}
