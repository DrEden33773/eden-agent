use super::*;
use eden_plugin_sdk::{
    CallContext,
    protocol::{coding::ToolDefinition, resources as r},
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
pub(super) struct Registry {
    selected: BTreeSet<String>,
    contributions: Vec<(String, String, bool)>,
    read_only: bool,
}
impl Registry {
    pub fn new(config: &Value) -> Result<Self, Fault> {
        let mut selected = match config.get("tools") {
            Some(value) => names(value, "tools")?,
            None => ["read", "write", "edit", "bash"].map(String::from).into(),
        };
        if let Some(excluded) = config.get("exclude_tools") {
            for name in names(excluded, "exclude_tools")? {
                selected.remove(&name);
            }
        }
        let read_only = match config.get("read_only") {
            None => false,
            Some(value) => value
                .as_bool()
                .ok_or_else(|| fault("InvalidInput", "read_only must be boolean"))?,
        };
        if read_only {
            for name in ["write", "edit", "bash", "powershell"] {
                selected.remove(name);
            }
        }
        let mut contributions = vec![];
        if let Some(entries) = config.get("contributions") {
            for entry in entries
                .as_array()
                .ok_or_else(|| fault("InvalidInput", "contributions must be an array"))?
            {
                contributions.push((
                    string(entry, "catalog")?.into(),
                    string(entry, "execute")?.into(),
                    entry
                        .get("read_only")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                ));
            }
        }
        Ok(Self {
            selected,
            contributions,
            read_only,
        })
    }
    pub async fn catalog(
        &self,
        cx: &CallContext,
        cwd: &str,
    ) -> Result<BTreeMap<String, (ToolDefinition, Option<String>)>, Fault> {
        let mut all: BTreeMap<_, _> = super::catalog::tools()
            .into_iter()
            .map(|tool| (tool.name.clone(), (tool, None)))
            .collect();
        let mut names: BTreeSet<_> = all.keys().cloned().collect();
        for (catalog, execute, read_only) in &self.contributions {
            let supplied: r::Catalog = cx
                .call(catalog, &r::CatalogRequest { cwd: cwd.into() })
                .await?;
            for tool in supplied.tools {
                if !names.insert(tool.name.clone()) {
                    return Err(fault(
                        "InvalidInput",
                        format!("duplicate tool contribution: {}", tool.name),
                    ));
                }
                if !self.read_only || *read_only {
                    all.insert(tool.name.clone(), (tool, Some(execute.clone())));
                }
            }
        }
        for name in &self.selected {
            if !names.contains(name) {
                return Err(fault(
                    "MissingDependency",
                    format!("selected tool is unavailable: {name}"),
                ));
            }
        }
        all.retain(|name, _| self.selected.contains(name));
        Ok(all)
    }
}
fn names(value: &Value, field: &str) -> Result<BTreeSet<String>, Fault> {
    let mut names = BTreeSet::new();
    for item in value
        .as_array()
        .ok_or_else(|| fault("InvalidInput", format!("{field} must be an array")))?
    {
        let name = item
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| fault("InvalidInput", format!("{field} entries must be names")))?;
        if !names.insert(name.into()) {
            return Err(fault(
                "InvalidInput",
                format!("duplicate {field} entry: {name}"),
            ));
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eden_plugin_sdk::serde_json::json;
    #[test]
    fn read_only_and_exclusions_apply_to_the_executable_selection() {
        let registry = Registry::new(&json!({"tools":["read","write","bash","grep"],"exclude_tools":["read"],"read_only":true})).unwrap();
        assert_eq!(registry.selected, BTreeSet::from(["grep".into()]));
        assert!(Registry::new(&json!({"tools":["read","read"]})).is_err());
    }
}
