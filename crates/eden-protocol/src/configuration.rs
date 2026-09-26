//! Shared configuration descriptions and value-only validation; no plugin initialization occurs here.
use crate::Fault;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Optional instance service; absence retains raw JSON editing and restart application.
pub const CONFIGURATION: &str = "eden.configuration.v1";

/// Describes ordinary configuration without granting installation or project trust.
/// Paths are RFC 6901 pointers; a live path includes its whole subtree.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
#[allow(missing_docs)] // Names carry the payload; semantics are documented below and in docs/configuration.md.
pub struct Description {
    /// None accepts raw JSON; an explicit schema uses only the documented subset.
    pub schema: Option<Value>,
    /// Informational defaults; validation never silently modifies the candidate.
    pub defaults: Value,
    pub description: Option<String>,
    pub secret_paths: Vec<String>,
    pub live_paths: Vec<String>,
    /// Empty means the host's existing editable layers, not read-only.
    pub editable_layers: Vec<Layer>,
}

/// Configuration provenance is independent of instance scope and lifecycle ownership.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Layer {
    Global,
    TrustedProject,
    Explicit,
}

/// Describe returns Description; Validate returns Validation without side effects.
/// Update returns Validation only after application completes: errors or Fault must leave
/// the old effective configuration unchanged. Initialization failure remains a service Fault.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum PluginRequest {
    Describe,
    Validate { config: Value },
    Update { config: Value },
}

/// Empty errors mean only that declared validation passed, never that initialization succeeded.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct Validation {
    pub errors: Vec<FieldError>,
}

/// Paths identify fields without echoing rejected values, including secrets.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct FieldError {
    pub path: String,
    pub code: String,
    pub message: String,
}

impl Validation {
    fn push(&mut self, path: &str, code: &str, message: &str) {
        self.errors.push(FieldError {
            path: path.into(),
            code: code.into(),
            message: message.into(),
        });
    }
}

/// Rejects invalid descriptors before checking values, including unused nested schemas.
/// An unsupported keyword is a descriptor Fault rather than a silently ignored constraint.
pub fn validate(description: &Description, config: &Value) -> Result<Validation, Fault> {
    for path in description
        .secret_paths
        .iter()
        .chain(&description.live_paths)
    {
        if !valid_pointer(path) {
            return Err(schema_fault());
        }
    }
    let mut result = Validation::default();
    if let Some(schema) = &description.schema {
        check_schema(schema)?;
        check_value(schema, config, "", &mut result);
    }
    Ok(result)
}

fn schema_fault() -> Fault {
    Fault::new(
        "configuration_schema",
        CONFIGURATION,
        "Invalid or unsupported configuration description",
    )
}

fn valid_pointer(path: &str) -> bool {
    if !path.is_empty() && !path.starts_with('/') {
        return false;
    }
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        if c == '~' && !matches!(chars.next(), Some('0' | '1')) {
            return false;
        }
    }
    true
}

fn child_path(path: &str, key: &str) -> String {
    format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"))
}

fn check_schema(schema: &Value) -> Result<(), Fault> {
    if schema.is_boolean() {
        return Ok(());
    }
    let object = schema.as_object().ok_or_else(schema_fault)?;
    for (key, value) in object {
        let valid = match key.as_str() {
            "type" => matches!(
                value.as_str(),
                Some("object" | "array" | "string" | "boolean" | "number" | "integer" | "null")
            ),
            "title" | "description" => value.is_string(),
            "default" => true,
            "enum" => value.as_array().is_some_and(|v| !v.is_empty()),
            "minimum" | "maximum" => value.is_number(),
            "minLength" | "maxLength" | "minItems" | "maxItems" => value.as_u64().is_some(),
            "additionalProperties" => value.is_boolean(),
            "required" => value
                .as_array()
                .is_some_and(|v| v.iter().all(Value::is_string)),
            "properties" => {
                let properties = value.as_object().ok_or_else(schema_fault)?;
                for schema in properties.values() {
                    check_schema(schema)?;
                }
                true
            }
            "items" => {
                check_schema(value)?;
                true
            }
            _ => false,
        };
        if !valid {
            return Err(schema_fault());
        }
    }
    Ok(())
}

fn check_value(schema: &Value, value: &Value, path: &str, result: &mut Validation) {
    if schema == &Value::Bool(false) {
        result.push(path, "schema", "Value is not allowed");
        return;
    }
    let Some(schema) = schema.as_object() else {
        return;
    };
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        let matches = match kind {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "number" => value.is_number(),
            "integer" => value.as_f64().is_some_and(|n| n.fract() == 0.0),
            "null" => value.is_null(),
            _ => false,
        };
        if !matches {
            result.push(path, "type", "Value has the wrong type");
            return;
        }
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && !options.contains(value)
    {
        result.push(path, "enum", "Value is not an allowed option");
    }
    if let Some(number) = value.as_f64() {
        for (keyword, below) in [("minimum", true), ("maximum", false)] {
            if let Some(bound) = schema.get(keyword).and_then(Value::as_f64)
                && if below {
                    number < bound
                } else {
                    number > bound
                }
            {
                result.push(path, keyword, "Number is outside the allowed range");
            }
        }
    }
    let length = value
        .as_str()
        .map(|s| (s.chars().count(), "minLength", "maxLength"))
        .or_else(|| value.as_array().map(|a| (a.len(), "minItems", "maxItems")));
    if let Some((length, min, max)) = length {
        for (keyword, below) in [(min, true), (max, false)] {
            if let Some(bound) = schema.get(keyword).and_then(Value::as_u64)
                && if below {
                    (length as u64) < bound
                } else {
                    (length as u64) > bound
                }
            {
                result.push(path, keyword, "Length is outside the allowed range");
            }
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    result.push(
                        &child_path(path, key),
                        "required",
                        "Required field is missing",
                    );
                }
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        for (key, child) in object {
            let child_path = child_path(path, key);
            if let Some(schema) = properties.and_then(|p| p.get(key)) {
                check_value(schema, child, &child_path, result);
            } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                result.push(
                    &child_path,
                    "additionalProperties",
                    "Undeclared field is not allowed",
                );
            }
        }
    }
    if let Some(array) = value.as_array()
        && let Some(items) = schema.get("items")
    {
        for (index, child) in array.iter().enumerate() {
            check_value(items, child, &child_path(path, &index.to_string()), result);
        }
    }
}

/// Keeps document shape while removing secret values. Null means redacted, not a patch.
/// Invalid pointers fail closed by redacting the entire document.
pub fn redact(value: &Value, secret_paths: &[String]) -> Value {
    let mut public = value.clone();
    for path in secret_paths {
        if !valid_pointer(path) {
            return Value::Null;
        }
        if let Some(secret) = public.pointer_mut(path) {
            *secret = Value::Null;
        }
    }
    public
}

/// Compares complete private candidates; public callers must retain old secrets privately.
/// Removal is also a secret edit and requires the separate private-input path.
pub fn validate_public_edit(description: &Description, old: &Value, new: &Value) -> Validation {
    let mut result = Validation::default();
    for path in &description.secret_paths {
        if !valid_pointer(path) || old.pointer(path) != new.pointer(path) {
            result.push(path, "secret_edit", "Secret changes require private input");
        }
    }
    result
}

/// Arrays are one replaceable value, matching the configuration merge contract.
/// Call only after descriptor and value validation; undeclared changes require restart.
pub fn is_live_change(description: &Description, old: &Value, new: &Value) -> bool {
    live_change_at(&description.live_paths, old, new, "")
}

fn live_change_at(paths: &[String], old: &Value, new: &Value, path: &str) -> bool {
    if old == new
        || paths.iter().any(|p| {
            valid_pointer(p)
                && (p == path
                    || path
                        .strip_prefix(p)
                        .is_some_and(|tail| tail.starts_with('/')))
        })
    {
        return true;
    }
    if let (Some(old), Some(new)) = (old.as_object(), new.as_object()) {
        return old.keys().chain(new.keys()).all(|key| {
            let child = child_path(path, key);
            match (old.get(key), new.get(key)) {
                (Some(a), Some(b)) => live_change_at(paths, a, b, &child),
                _ => paths.iter().any(|p| {
                    valid_pointer(p)
                        && (p == &child
                            || child
                                .strip_prefix(p)
                                .is_some_and(|tail| tail.starts_with('/')))
                }),
            }
        });
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn string_boolean_array_and_object_constraints_are_checked() {
        let description = Description {
            schema: Some(json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["missing"],
                "properties": {
                    "text": { "type": "string", "minLength": 2 },
                    "flag": { "type": "boolean" },
                    "list": {
                        "type": "array",
                        "maxItems": 1,
                        "items": { "type": "number", "maximum": 5 },
                    },
                    "nested": { "type": "object" },
                },
            })),
            ..Description::default()
        };
        let result = validate(
            &description,
            &json!({ "text": "猫", "flag": 0, "list": [1, 6], "nested": false, "extra": true }),
        )
        .unwrap();
        assert_eq!(
            result
                .errors
                .iter()
                .map(|error| error.code.as_str())
                .collect::<Vec<_>>(),
            vec![
                "required",
                "additionalProperties",
                "type",
                "maxItems",
                "maximum",
                "type",
                "minLength"
            ]
        );
    }

    #[test]
    fn malformed_pointers_fail_closed_and_root_secrets_are_supported() {
        let description = Description {
            secret_paths: vec!["/bad~2".into()],
            ..Description::default()
        };
        assert!(validate(&description, &json!({})).is_err());
        assert_eq!(
            redact(&json!({ "secret": 42 }), &description.secret_paths),
            Value::Null
        );
        assert_eq!(
            redact(&json!({ "secret": 42 }), &[String::new()]),
            Value::Null
        );
    }

    #[test]
    fn service_requests_have_stable_wire_operations() {
        let request = PluginRequest::Validate {
            config: json!({ "enabled": true }),
        };
        let wire = serde_json::to_value(&request).unwrap();
        assert_eq!(
            wire,
            json!({ "op": "validate", "config": { "enabled": true } })
        );
        assert_eq!(
            serde_json::from_value::<PluginRequest>(wire).unwrap(),
            request
        );
    }

    #[test]
    fn nested_validation_reports_pointers_without_values() {
        let description = Description {
            schema: Some(json!({
                "type": "object",
                "required": ["mode"],
                "properties": {
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "n": { "type": "integer", "minimum": 1 } },
                        },
                    },
                    "mode": { "enum": ["idle", "off"] },
                },
            })),
            ..Description::default()
        };
        let result = validate(
            &description,
            &json!({ "items": [{ "n": 0 }], "mode": "secret-invalid-value" }),
        )
        .unwrap();
        assert_eq!(
            result
                .errors
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/items/0/n", "/mode"]
        );
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("secret-invalid-value")
        );
    }

    #[test]
    fn unsupported_schema_is_rejected_even_for_missing_properties() {
        let description = Description {
            schema: Some(json!({ "properties": { "unused": { "pattern": ".*" } } })),
            ..Description::default()
        };
        assert_eq!(
            validate(&description, &json!({})).unwrap_err().code,
            "configuration_schema"
        );
    }

    #[test]
    fn raw_json_fallback_accepts_arbitrary_values() {
        assert!(
            validate(&Description::default(), &json!([true, { "raw": 42 }]))
                .unwrap()
                .errors
                .is_empty()
        );
    }

    #[test]
    fn secrets_are_redacted_and_public_changes_rejected() {
        let description = Description {
            secret_paths: vec!["/nested/a~1b".into(), "/array/0".into()],
            ..Description::default()
        };
        let original = json!({ "nested": { "a/b": "secret" }, "array": ["private", 42] });
        assert_eq!(
            redact(&original, &description.secret_paths),
            json!({ "nested": { "a/b": null }, "array": [null, 42] })
        );
        assert!(
            validate_public_edit(&description, &original, &original)
                .errors
                .is_empty()
        );
        assert_eq!(
            validate_public_edit(&description, &original, &json!({}))
                .errors
                .len(),
            2
        );
    }

    #[test]
    fn only_declared_live_subtrees_allow_in_place_changes() {
        let description = Description {
            live_paths: vec!["/live".into()],
            ..Description::default()
        };
        assert!(is_live_change(
            &description,
            &json!({ "live": { "n": 1 }, "restart": 0 }),
            &json!({ "live": { "n": 2 }, "restart": 0 })
        ));
        assert!(!is_live_change(
            &description,
            &json!({ "live": 1 }),
            &json!({ "live": 2, "restart": 0 })
        ));
    }
}
