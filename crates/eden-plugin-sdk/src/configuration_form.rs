//! Default configuration controls return ordinary presentation nodes for author customization.
use crate::protocol::{configuration::Description, configuration_form as f, presentation as p};
use serde_json::Value;
use std::collections::BTreeMap;

/// Optional management service: accepts Model and returns an ordinary presentation View.
pub const FORM: &str = "eden.configuration.presentation.v1";

/// Inputs come from the host inspection; no private values belong in this public model.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Model {
    /// Authoritative target identity retained when adjusting the returned presentation.
    pub binding: f::Binding,
    /// Existing A2 types and constraints, shared with headless callers.
    pub description: Description,
    /// Redacted effective values, never private configuration.
    pub effective: Value,
    /// Per-pointer provenance from configuration inspection.
    pub sources: BTreeMap<String, String>,
    /// Presence only; private input never starts with a saved secret value.
    pub secrets_configured: BTreeMap<String, bool>,
}

/// Build a portable form without initializing a plugin or creating another configuration authority.
/// Authors may arrange nodes, labels and groups; retain bindings and field paths for host validation.
pub fn default_form(model: &Model) -> p::View {
    let mut fields = vec![];
    let schema = model.description.schema.as_ref().unwrap_or(&Value::Null);
    fields_for(model, schema, "", &mut fields);
    if model
        .description
        .profile
        .is_some_and(|version| version != 1)
    {
        for field in &mut fields {
            field.writable = false;
            field.description =
                Some("Unsupported configuration profile; use headless recovery".into());
        }
    }
    p::View::new(
        format!("configuration-{}", model.binding.instance),
        p::Slot::Panel,
        format!("Configuration: {}", model.binding.instance),
    )
    .node(p::Node::ConfigurationForm {
        id: "settings".into(),
        binding: model.binding.clone(),
        fields,
    })
}

fn fields_for(model: &Model, schema: &Value, path: &str, fields: &mut Vec<f::Field>) {
    let secret = model
        .description
        .secret_paths
        .iter()
        .any(|p| path == p || path.starts_with(&format!("{p}/")));
    let secret_child = model
        .description
        .secret_paths
        .iter()
        .any(|p| p.starts_with(&format!("{path}/")));
    if !secret && let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (key, child) in properties {
            fields_for(
                model,
                child,
                &format!("{path}/{}", key.replace('~', "~0").replace('/', "~1")),
                fields,
            );
        }
        // Unknown non-secret members remain available and untouched unless explicitly edited.
        if let Some(object) = model.effective.pointer(path).and_then(Value::as_object) {
            for key in object.keys().filter(|key| !properties.contains_key(*key)) {
                fields_for(
                    model,
                    &Value::Null,
                    &format!("{path}/{}", key.replace('~', "~0").replace('/', "~1")),
                    fields,
                );
            }
        }
        return;
    }
    let control = if secret {
        f::Control::Secret
    } else if schema.get("enum").is_some() {
        f::Control::Choice
    } else {
        match schema.get("type").and_then(Value::as_str) {
            Some("string") => f::Control::Text,
            Some("boolean") => f::Control::Boolean,
            Some("integer") => f::Control::Integer,
            Some("number") => f::Control::Number,
            Some("array") => f::Control::List,
            _ => f::Control::Json,
        }
    };
    fields.push(f::Field {
        path: path.into(),
        label: schema
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(if path.is_empty() {
                "Configuration (JSON fallback)"
            } else {
                path
            })
            .into(),
        description: if secret {
            Some(
                "Set, replace or clear through private input; current values are never shown"
                    .into(),
            )
        } else {
            schema
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned)
        },
        control,
        value: if secret || secret_child {
            None
        } else {
            model.effective.pointer(path).cloned()
        },
        options: if secret || secret_child {
            vec![]
        } else {
            schema
                .get("enum")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        },
        item_kind: schema
            .pointer("/items/type")
            .and_then(Value::as_str)
            .map(str::to_owned),
        source: model.sources.get(path).cloned(),
        writable: !secret_child
            && (model.description.editable_layers.is_empty()
                || model
                    .description
                    .editable_layers
                    .contains(&crate::protocol::configuration::Layer::Explicit)),
        configured: model.secrets_configured.get(path).copied().unwrap_or(false),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn helper_returns_adjustable_presentation_and_never_secret_defaults() {
        let model = Model {
            binding: f::Binding {
                instance: "example".into(),
                generation: Some(1),
                revision: 3,
                profile: 1,
            },
            description: Description {
                schema: Some(json!({
                    "properties": {
                        "count": { "type": "integer" },
                        "key": { "type": "string", "default": "CANARY", "enum": ["CANARY"] },
                    },
                })),
                secret_paths: vec!["/key".into()],
                ..Description::default()
            },
            effective: json!({ "count": 3, "key": null, "unknown": true }),
            sources: BTreeMap::new(),
            secrets_configured: BTreeMap::from([("/key".into(), true)]),
        };
        let view = default_form(&model);
        assert!(!serde_json::to_string(&view).unwrap().contains("CANARY"));
        let p::Node::ConfigurationForm { fields, .. } = &view.nodes[0] else {
            panic!("expected form")
        };
        assert_eq!(fields.len(), 3);
        assert!(
            fields
                .iter()
                .any(|f| f.path == "/count" && f.control == f::Control::Integer)
        );
    }
    #[test]
    fn raw_values_and_unknown_profiles_have_explicit_fallbacks() {
        let mut model = Model {
            binding: f::Binding {
                instance: "legacy".into(),
                generation: None,
                revision: 0,
                profile: 1,
            },
            description: Description::default(),
            effective: serde_json::json!({ "unknown": [1, 2] }),
            sources: BTreeMap::new(),
            secrets_configured: BTreeMap::new(),
        };
        let view = default_form(&model);
        let p::Node::ConfigurationForm { fields, .. } = &view.nodes[0] else {
            panic!("form")
        };
        assert_eq!(fields[0].control, f::Control::Json);
        assert!(fields[0].writable);
        model.description.profile = Some(99);
        model.binding.profile = 99;
        let view = default_form(&model);
        let p::Node::ConfigurationForm { fields, .. } = &view.nodes[0] else {
            panic!("form")
        };
        assert!(fields.iter().all(|field| !field.writable));
        assert!(
            fields[0]
                .description
                .as_ref()
                .unwrap()
                .contains("Unsupported")
        );
    }
}
