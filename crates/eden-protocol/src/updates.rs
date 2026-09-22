//! Update discovery is separate from preparation and activation so startup cannot install code.
use serde::{Deserialize, Serialize};

/// Replaceable update provider; requests use only serializable SDK data.
pub const UPDATE_SOURCE: &str = "eden.update-source.v1";

/// Stable excludes preview releases; prerelease includes both; a tag is exact.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Channel {
    #[default]
    Stable,
    Prerelease,
    Tag {
        tag: String,
    },
}

/// The host and each installed plugin may have independent tracking policies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum UpdateTarget {
    Host,
    Plugin { name: String },
}

/// A check result binds the exact downloadable bytes, not a moving release name.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Candidate {
    pub target: UpdateTarget,
    pub version: String,
    pub source: serde_json::Value,
    pub channel: Channel,
}

/// Prepared bytes are inert until a separate activation request.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct PreparedUpdate {
    pub target: UpdateTarget,
    pub version: String,
    pub path: String,
    pub digest: String,
}

/// Unconfigured sources stay visible instead of being mistaken for up-to-date installs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct UpdateStatus {
    pub target: UpdateTarget,
    pub current_version: Option<String>,
    pub configured: bool,
    pub managed: bool,
    pub instructions: String,
    pub candidate: Option<Candidate>,
}

/// Only Check performs discovery networking; Prepare and Activate require explicit callers.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum UpdateRequest {
    Discover,
    Check {
        target: UpdateTarget,
        #[serde(default)]
        channel: Channel,
    },
    Prepare {
        candidate: Candidate,
    },
    Activate {
        prepared: PreparedUpdate,
    },
}

/// Activation affects future launches; running sessions keep their existing bindings.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum UpdateReply {
    Discovered {
        targets: Vec<UpdateStatus>,
    },
    Checked {
        status: UpdateStatus,
    },
    Prepared {
        prepared: PreparedUpdate,
    },
    Activated {
        path: String,
        previous: Option<String>,
    },
}

/// Resolve the last committed activation without loading any native plugin.
/// The append-only journal permits atomic publication on Windows while retaining rollback entries.
pub fn active_installation(root: &std::path::Path) -> std::io::Result<Option<std::path::PathBuf>> {
    let directory = root.join("activations");
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut records = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
            && let Some(sequence) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| s.len() == 20)
                .and_then(|s| s.parse::<u64>().ok())
        {
            records.push((sequence, path));
        }
    }
    records.sort_by_key(|(sequence, _)| *sequence);
    let Some((_, record)) = records.pop() else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(record)?)?;
    let path = value["path"]
        .as_str()
        .ok_or_else(|| std::io::Error::other("activation has no path"))?;
    let path = std::path::Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        || !path.starts_with("releases")
    {
        return Err(std::io::Error::other("activation escapes managed releases"));
    }
    let installation = root.join(path);
    if !installation.is_dir() {
        return Err(std::io::Error::other("active installation is missing"));
    }
    Ok(Some(installation))
}
