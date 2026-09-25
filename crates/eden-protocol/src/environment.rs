//! Host-resolved inputs available equally to native and embedded authors.
use crate::{Fault, resources::LockedResourcePackage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// Reserved factory configuration key. The host replaces it after merging user
/// settings; a composition cannot use this key to override host decisions.
pub const CONFIG_KEY: &str = "__eden_host";

/// Discovery results for a package factory, independent of its name or services.
/// Paths are host-local absolute paths. Settings exclude untrusted project input;
/// `commands_trusted` permits commands from those accepted configuration sources,
/// whereas `project_trusted` governs loading project resources. This is input
/// authority, not a native-code sandbox or a grant of filesystem isolation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct HostEnvironment {
    pub cwd: PathBuf,
    pub global_dir: PathBuf,
    pub project_trusted: bool,
    pub commands_trusted: bool,
    pub settings: Value,
    pub history_path: Option<PathBuf>,
    pub managed_root: Option<PathBuf>,
    #[serde(default)]
    pub resource_packages: Vec<LockedResourcePackage>,
}

impl HostEnvironment {
    /// Absence supports directly constructed factories and older standalone test
    /// hosts. A present but invalid envelope is an error, never a reason to fall
    /// back to potentially conflicting legacy configuration fields.
    pub fn from_config(config: &Value) -> Result<Option<Self>, Fault> {
        config
            .get(CONFIG_KEY)
            .map(|value| {
                serde_json::from_value(value.clone()).map_err(|error| {
                    Fault::new("InvalidInput", "host-environment", error.to_string())
                })
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_environment_does_not_fall_back_to_legacy_fields() {
        let config = serde_json::json!({ "__eden_host": null, "cwd": "/legacy" });
        assert_eq!(
            HostEnvironment::from_config(&config).unwrap_err().code,
            "InvalidInput"
        );
        assert!(
            HostEnvironment::from_config(&serde_json::json!({ "cwd": "/legacy" }))
                .unwrap()
                .is_none()
        );
    }
}
