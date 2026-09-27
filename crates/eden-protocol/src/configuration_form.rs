//! Shared configuration bindings; values are edited through the configuration authority.
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Configuration identity is separate from live view revision and background status updates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Binding {
    pub instance: String,
    pub generation: Option<u64>,
    pub revision: u64,
    pub profile: u32,
}

/// An edit distinguishes an explicit null, removal, and restoring the inherited value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Edit {
    Set { path: String, value: Value },
    Clear { path: String },
    Inherit { path: String },
}

/// Portable controls do not reinterpret configuration schemas in each frontend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Control {
    Text,
    Boolean,
    Integer,
    Number,
    Choice,
    List,
    Json,
    Secret,
}

/// Non-secret starting values and source information for one stable JSON pointer.
/// Secret controls carry only presence and a disabled private-input entry state in D1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Field {
    pub path: String,
    pub label: String,
    pub description: Option<String>,
    pub control: Control,
    pub value: Option<Value>,
    pub options: Vec<Value>,
    #[serde(default)]
    pub item_kind: Option<String>,
    pub source: Option<String>,
    pub writable: bool,
    pub configured: bool,
}

/// Reusable management actions share the host's validation, preview, and apply transactions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Submission {
    pub binding: Binding,
    pub edits: Vec<Edit>,
}

impl Edit {
    /// RFC 6901 target; arrays are edited as complete values to retain A2 merge semantics.
    pub fn path(&self) -> &str {
        match self {
            Self::Set { path, .. } | Self::Clear { path } | Self::Inherit { path } => path,
        }
    }
}

/// Apply exact non-secret path edits to a private candidate before authoritative validation.
/// Inherit uses the session's original effective configuration, never schema defaults.
/// Object keys may be created; arrays must be replaced as a whole. Invalid input is atomic.
pub fn apply_edits(
    value: &mut Value,
    inherited: &Value,
    edits: &[Edit],
) -> Result<(), crate::Fault> {
    let invalid = || {
        crate::Fault::new(
            "InvalidInput",
            "configuration",
            "invalid configuration edit path",
        )
    };
    let mut candidate = value.clone();
    for edit in edits {
        let path = edit.path();
        if !path.is_empty() && !path.starts_with('/') {
            return Err(invalid());
        }
        let mut chars = path.chars();
        while let Some(c) = chars.next() {
            if c == '~' && !matches!(chars.next(), Some('0' | '1')) {
                return Err(invalid());
            }
        }
        let replacement = match edit {
            Edit::Set { value, .. } => Some(value.clone()),
            Edit::Clear { .. } => None,
            Edit::Inherit { .. } => inherited.pointer(path).cloned(),
        };
        if path.is_empty() {
            candidate = replacement.unwrap_or(Value::Null);
            continue;
        }
        let mut parts = path[1..]
            .split('/')
            .map(|s| s.replace("~1", "/").replace("~0", "~"))
            .peekable();
        let mut parent = &mut candidate;
        while let Some(key) = parts.next() {
            let object = parent.as_object_mut().ok_or_else(invalid)?;
            if parts.peek().is_none() {
                if let Some(replacement) = replacement {
                    object.insert(key, replacement);
                } else {
                    object.remove(&key);
                }
                break;
            }
            parent = object.entry(key).or_insert_with(|| serde_json::json!({}));
        }
    }
    *value = candidate;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn edits_distinguish_null_clear_and_inherit() {
        let inherited = json!({ "nested": { "n": 7 } });
        let mut current = json!({ "nested": { "n": 9, "extra": 3 }, "unknown": true });
        apply_edits(
            &mut current,
            &inherited,
            &[Edit::Set {
                path: "/nested/n".into(),
                value: Value::Null,
            }],
        )
        .unwrap();
        assert_eq!(current.pointer("/nested/n"), Some(&Value::Null));
        apply_edits(
            &mut current,
            &inherited,
            &[Edit::Clear {
                path: "/nested/n".into(),
            }],
        )
        .unwrap();
        assert_eq!(current.pointer("/nested/n"), None);
        apply_edits(
            &mut current,
            &inherited,
            &[Edit::Inherit {
                path: "/nested/n".into(),
            }],
        )
        .unwrap();
        assert_eq!(
            current,
            json!({ "nested": { "n": 7, "extra": 3 }, "unknown": true })
        );
    }
    #[test]
    fn rejects_invalid_paths_and_array_element_mutations() {
        let mut value = json!({ "a": [1, 2] });
        for path in ["/a/0", "/a~3"] {
            assert!(
                apply_edits(
                    &mut value,
                    &Value::Null,
                    &[Edit::Clear { path: path.into() }]
                )
                .is_err()
            );
        }
    }
}
