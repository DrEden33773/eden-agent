//! Data-only provenance and installed inventory; neither operation loads disabled native code.
use super::*;
use serde_json::Value;
use std::collections::BTreeMap;

fn mark(value: &Value, path: &str, layer: &str, origins: &mut BTreeMap<String, String>) {
    if let Some(object) = value.as_object() {
        origins.remove(path);
        for (key, value) in object {
            if key == eden_protocol::environment::CONFIG_KEY {
                continue;
            }
            let path = format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"));
            mark(value, &path, layer, origins);
        }
    } else {
        origins.retain(|key, _| key != path && !key.starts_with(&format!("{path}/")));
        origins.insert(path.into(), layer.into());
    }
}
pub(crate) fn overlay(origins: &mut BTreeMap<String, String>, patch: &Value, layer: &str) {
    mark(patch, "", layer, origins);
}
fn plugin_settings(path: &Path, package: &str) -> Result<Value, Fault> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Value::Null),
        Err(e) => return Err(Fault::new("FileFailure", "configuration", e.to_string())),
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Fault::new("InvalidInput", "configuration", e.to_string()))?;
    Ok(value["plugins"][package].clone())
}
pub(crate) fn origins(
    composition: &eden_protocol::Composition,
    options: &WorkspaceOptions,
) -> Result<BTreeMap<String, BTreeMap<String, String>>, Fault> {
    let mut result = BTreeMap::new();
    for spec in configuration::specs(composition) {
        let (_, value) = configuration::config(composition, &spec.id)?;
        let mut origins = BTreeMap::new();
        overlay(&mut origins, &value, "composition");
        if let Some(environment) = &composition.host_environment {
            let global =
                plugin_settings(&environment.global_dir.join("settings.json"), &spec.package)?;
            if !global.is_null() {
                overlay(&mut origins, &global, "global");
            }
            if environment.project_trusted {
                let project =
                    plugin_settings(&environment.cwd.join(".eden/settings.json"), &spec.package)?;
                if !project.is_null() {
                    overlay(&mut origins, &project, "trusted_project");
                }
            }
        }
        if let Some(value) = options
            .overrides
            .get("plugins")
            .and_then(|v| v.get(&spec.package))
        {
            overlay(&mut origins, value, "explicit_workspace");
        }
        if let Some(value) = &spec.config {
            origins.clear();
            overlay(&mut origins, value, "explicit_instance");
        }
        result.insert(spec.id, origins);
    }
    Ok(result)
}

/// Installation status is data-only; dependency diagnostics do not activate an installed package.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(missing_docs)]
pub struct InstalledPackage {
    pub package: String,
    pub version: String,
    pub path: String,
    pub source: Value,
    pub state: String,
    pub missing_dependencies: Vec<String>,
}
pub(crate) fn installed(
    composition: &eden_protocol::Composition,
) -> Result<Vec<InstalledPackage>, Fault> {
    let Some(environment) = &composition.host_environment else {
        return Ok(vec![]);
    };
    let root = environment.global_dir.join("distribution/packages");
    if !root.exists() {
        return Ok(vec![]);
    }
    fn visit(
        path: &Path,
        composition: &eden_protocol::Composition,
        result: &mut Vec<InstalledPackage>,
    ) -> Result<(), Fault> {
        let failure =
            |e: std::io::Error| Fault::new("FileFailure", "configuration-inventory", e.to_string());
        let receipt = path.join("receipt.json");
        if receipt.is_file() {
            let value: Value = serde_json::from_slice(&std::fs::read(receipt).map_err(failure)?)
                .map_err(|e| {
                    Fault::new("InvalidInput", "configuration-inventory", e.to_string())
                })?;
            let manifest = &value["manifest"];
            let package = manifest["descriptor"]["package"]
                .as_str()
                .or(value["resources"]["name"].as_str())
                .unwrap_or_default();
            let version = manifest["descriptor"]["version"]
                .as_str()
                .or(value["resources"]["version"].as_str())
                .unwrap_or_default();
            let missing: Vec<String> = manifest["requires"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|role| !composition.roles.contains_key(*role))
                .map(str::to_owned)
                .collect();
            let enabled = composition
                .packages
                .iter()
                .any(|m| m.descriptor.package == package && m.descriptor.version == version);
            result.push(InstalledPackage {
                package: package.into(),
                version: version.into(),
                path: path.to_string_lossy().into_owned(),
                source: value["source"].clone(),
                state: if enabled {
                    "enabled"
                } else if missing.is_empty() {
                    "disabled"
                } else {
                    "dependency_missing"
                }
                .into(),
                missing_dependencies: missing,
            });
            return Ok(());
        }
        for entry in std::fs::read_dir(path).map_err(failure)? {
            let entry = entry.map_err(failure)?;
            if entry.file_type().map_err(failure)?.is_dir() {
                visit(&entry.path(), composition, result)?;
            }
        }
        Ok(())
    }
    let mut result = vec![];
    visit(&root, composition, &mut result)?;
    result.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn field_origins_follow_recursive_objects_and_replace_arrays() {
        let mut origins = BTreeMap::new();
        overlay(
            &mut origins,
            &json!({ "nested": { "left": 1, "right": 2 }, "list": [1, 2] }),
            "global",
        );
        overlay(
            &mut origins,
            &json!({ "nested": { "left": 3 }, "list": [3] }),
            "trusted_project",
        );
        overlay(
            &mut origins,
            &json!({ "nested": { "right": 4 } }),
            "explicit_session",
        );
        assert_eq!(
            origins,
            BTreeMap::from([
                ("/nested/left".into(), "trusted_project".into()),
                ("/nested/right".into(), "explicit_session".into()),
                ("/list".into(), "trusted_project".into())
            ])
        );
        overlay(
            &mut origins,
            &json!({ "nested": false }),
            "explicit_session",
        );
        assert!(!origins.contains_key("/nested/left"));
        assert_eq!(origins["/nested"], "explicit_session");
    }
}
