//! Shared request-edit validation. A complete candidate is checked before any durable write.
use crate::{
    Fault,
    coding::{Item, ModelInput},
};
use std::collections::BTreeSet;

/// Reject incomplete or reordered tool groups before a candidate can become model input.
/// Historical execution remains separate; this validates only the effective request.
pub fn validate(input: &ModelInput) -> Result<(), Fault> {
    let mut calls = BTreeSet::new();
    let mut pending = BTreeSet::new();
    for item in &input.items {
        match item {
            Item::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                if call_id.is_empty() || name.trim().is_empty() || !calls.insert(call_id) {
                    return Err(invalid("empty or duplicate tool call identity"));
                }
                serde_json::from_str::<serde_json::Value>(arguments)
                    .map_err(|_| invalid("tool arguments must be valid JSON"))?;
                pending.insert(call_id);
            }
            Item::ToolResult { call_id, .. } => {
                if !pending.remove(call_id) {
                    return Err(invalid("tool result must follow its unique call"));
                }
            }
            Item::Message { role, .. } => {
                if !matches!(role.as_str(), "system" | "developer" | "user" | "assistant") {
                    return Err(invalid("unknown message role"));
                }
                if !pending.is_empty() {
                    return Err(invalid("tool group cannot be split by a message"));
                }
            }
            Item::ProviderState { .. } => {}
        }
    }
    if !pending.is_empty() {
        return Err(invalid("tool calls and results must be edited together"));
    }
    let mut names = BTreeSet::new();
    for tool in &input.tools {
        if tool.name.trim().is_empty() || !names.insert(&tool.name) || !tool.parameters.is_object()
        {
            return Err(invalid(
                "tool declarations require unique names and object schemas",
            ));
        }
    }
    Ok(())
}
fn invalid(message: &str) -> Fault {
    Fault::new("InvalidContext", "context-edit", message)
}

/// Stable identities distinguish original records from inserted and synthesized input.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(missing_docs)]
pub struct Entry {
    pub id: String,
    pub item: Item,
    #[serde(default)]
    pub references: Vec<crate::session_reference::Reference>,
}
/// A versioned editable request; target selection remains owned by the session.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(missing_docs)]
pub struct Document {
    pub entries: Vec<Entry>,
    pub tools: Vec<crate::coding::ToolDefinition>,
}
/// Persisted edits survive reopening; request edits are claimed by one logical request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Scope {
    #[default]
    Branch,
    NextRequest,
}
/// The captured store revision prevents one frontend from overwriting another's draft.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(missing_docs)]
pub struct Revision {
    pub session_id: u64,
    pub sequence: u64,
    pub head: Option<u64>,
    pub branch: String,
}
/// Inspection never claims temporary edits or contacts a model.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(missing_docs)]
pub struct Snapshot {
    pub revision: Revision,
    pub original: Document,
    pub effective: Document,
    pub edits: Vec<u64>,
    pub last_request: Option<ModelInput>,
    #[serde(default)]
    pub policies: Vec<Policy>,
    #[serde(default)]
    pub model: Option<crate::models::ModelTarget>,
    #[serde(default)]
    pub budget: serde_json::Value,
    #[serde(default)]
    pub image_limits: serde_json::Value,
}
/// Submit the reviewed version and complete candidate together; failures commit nothing.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(missing_docs)]
pub struct Apply {
    pub revision: Revision,
    pub document: Document,
    #[serde(default)]
    pub scope: Scope,
    pub source: String,
}
/// A durable structural replacement affects only captured identities. New descendants remain.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(missing_docs)]
pub struct Edit {
    pub base: Vec<String>,
    pub replacement: Vec<Entry>,
    pub unchanged: Vec<String>,
    pub tools: Option<Vec<crate::coding::ToolDefinition>>,
    pub scope: Scope,
    pub source: String,
}
impl Document {
    /// Preserve the authoritative target and output allowance while changing visible input.
    pub fn replace_input(&self, input: &mut ModelInput) {
        input.items = self
            .entries
            .iter()
            .map(|entry| entry.item.clone())
            .collect();
        input.tools = self.tools.clone();
    }
    /// Validate identities in addition to the provider-independent message structure.
    pub fn validate(&self) -> Result<(), Fault> {
        let mut ids = BTreeSet::new();
        for entry in &self.entries {
            if entry.id.is_empty() || !ids.insert(&entry.id) {
                return Err(invalid(
                    "context entries require unique nonempty identities",
                ));
            }
        }
        validate(&ModelInput {
            target: None,
            max_output_tokens: None,
            items: self
                .entries
                .iter()
                .map(|entry| entry.item.clone())
                .collect(),
            tools: self.tools.clone(),
        })
    }
    /// Replacements are atomic and do not discard entries appended after the captured view.
    pub fn apply(&self, edit: &Edit) -> Result<Self, Fault> {
        let base: BTreeSet<_> = edit.base.iter().map(String::as_str).collect();
        let unchanged: BTreeSet<_> = edit.unchanged.iter().map(String::as_str).collect();
        let current: std::collections::BTreeMap<_, _> = self
            .entries
            .iter()
            .map(|entry| (entry.id.as_str(), entry))
            .collect();
        if base.len() != edit.base.len()
            || base
                .iter()
                .any(|id| !unchanged.contains(id) && !current.contains_key(id))
        {
            return Err(Fault::new(
                "ContextConflict",
                "context-edit",
                "edited source entries are no longer available; preview again",
            ));
        }
        let mut entries = Vec::new();
        for entry in &edit.replacement {
            if unchanged.contains(entry.id.as_str()) {
                if let Some(current) = current.get(entry.id.as_str()) {
                    entries.push((*current).clone());
                }
            } else {
                entries.push(entry.clone());
            }
        }
        entries.extend(
            self.entries
                .iter()
                .filter(|entry| !base.contains(entry.id.as_str()))
                .cloned(),
        );
        let candidate = Self {
            entries,
            tools: edit.tools.clone().unwrap_or_else(|| self.tools.clone()),
        };
        candidate.validate()?;
        Ok(candidate)
    }
}

/// Replaceable shared editing service; the host supplies the current resource/model facts.
pub const SERVICE: &str = "eden.context-edit.v1";
/// Inspection and edits share the same projection inputs used by the coding loop.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Request {
    Inspect {
        input: crate::coding::ContextInput,
    },
    ProjectSource {
        input: crate::coding::ContextInput,
    },
    Images {
        input: crate::coding::ContextInput,
        edit: ImageEdit,
    },
    CheckInput {
        input: crate::coding::ContextInput,
        content: Vec<crate::coding::Block>,
        references: Vec<crate::session_reference::Reference>,
    },
    Rebuild {
        input: crate::coding::ContextInput,
        rebuild: Rebuild,
    },
    Apply {
        input: crate::coding::ContextInput,
        edit: Apply,
    },
}

/// Prepared input carries the exact revision and edit claims that must commit before sending.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(missing_docs)]
pub struct Prepared {
    pub input: ModelInput,
    pub revision: Revision,
    pub edits: Vec<u64>,
    pub image_records: Vec<crate::coding::RecordDraft>,
    pub references: bool,
    pub prompt_cache: serde_json::Value,
}

/// Policy boundaries are explicit and never run on an inspector read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Boundary {
    BeforeRequest,
    ToolRoundEnd,
    RunFinish,
}
/// Each policy consumes the previous policy's validated document in configured order.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(missing_docs)]
pub struct PolicyInput {
    #[serde(default)]
    pub config: serde_json::Value,
    pub boundary: Boundary,
    pub document: Document,
    pub revision: Revision,
    pub protected: Vec<String>,
}
/// Persistent policies create the same durable transactions as manual editors.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(missing_docs)]
pub struct PolicyOutput {
    pub document: Document,
    pub scope: Scope,
}
/// The role is a registered native service, or the bundled `large_tool_output` policy.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(missing_docs)]
pub struct Policy {
    pub name: String,
    pub role: String,
    pub boundary: Boundary,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Rebuild from original execution records on a fresh branch without replaying tools.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(missing_docs)]
pub struct Rebuild {
    pub revision: Revision,
    pub branch: String,
    pub edit_ids: Vec<u64>,
}

/// Image changes are explicit and create a new retained version rather than overwriting history.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum ImageAction {
    Preserve,
    Omit,
    ReAdapt,
}
/// Entry and original block index identify the previewed image even after an omission.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(missing_docs)]
pub struct ImageEdit {
    pub revision: Revision,
    pub choices: std::collections::BTreeMap<String, std::collections::BTreeMap<usize, ImageAction>>,
}

/// Copies renumber local tree nodes; frozen cross-session source identities remain untouched.
pub fn remap_record(
    kind: &str,
    payload: &mut serde_json::Value,
    mapping: &std::collections::BTreeMap<u64, u64>,
    public_only: bool,
) {
    fn identity(value: &mut serde_json::Value, mapping: &std::collections::BTreeMap<u64, u64>) {
        let Some(id) = value.as_str() else {
            return;
        };
        let mut parts = id.splitn(3, ':');
        let Some(prefix) = parts.next() else {
            return;
        };
        if !matches!(prefix, "record" | "extension") {
            return;
        }
        let Some(source) = parts.next().and_then(|value| value.parse::<u64>().ok()) else {
            return;
        };
        let suffix = parts.next().unwrap_or("0");
        *value = mapping
            .get(&source)
            .map_or(serde_json::Value::Null, |mapped| {
                serde_json::Value::String(format!("{prefix}:{mapped}:{suffix}"))
            });
    }
    fn ids(value: &mut serde_json::Value, mapping: &std::collections::BTreeMap<u64, u64>) {
        if let Some(values) = value.as_array_mut() {
            for value in values.iter_mut() {
                identity(value, mapping);
            }
            values.retain(|value| !value.is_null());
        }
    }
    fn entries(
        value: &mut serde_json::Value,
        mapping: &std::collections::BTreeMap<u64, u64>,
        public_only: bool,
    ) {
        if let Some(entries) = value.as_array_mut() {
            for entry in entries.iter_mut() {
                identity(&mut entry["id"], mapping);
            }
            entries.retain(|entry| {
                !entry["id"].is_null()
                    && !(public_only && entry["item"]["type"] == "provider_state")
            });
        }
    }
    fn sequences(value: &mut serde_json::Value, mapping: &std::collections::BTreeMap<u64, u64>) {
        if let Some(values) = value.as_array_mut() {
            *values = values
                .iter()
                .filter_map(|value| {
                    value
                        .as_u64()
                        .and_then(|source| mapping.get(&source))
                        .map(|id| serde_json::json!(id))
                })
                .collect();
        }
    }
    match kind {
        "context_edit" => {
            ids(&mut payload["base"], mapping);
            ids(&mut payload["unchanged"], mapping);
            entries(&mut payload["replacement"], mapping, public_only);
        }
        "compaction" => {
            if let Some(value) = payload.get_mut("context_retained") {
                entries(value, mapping, public_only);
            }
            if let Some(value) = payload.get_mut("context_system") {
                entries(value, mapping, public_only);
            }
            if let Some(value) = payload.get_mut("context_protected") {
                ids(value, mapping);
            }
            if let Some(value) = payload.get_mut("context_edits") {
                sequences(value, mapping);
            }
        }
        "context_rebuild" => {
            sequences(&mut payload["edit_ids"], mapping);
            if let Some(source) = payload["source_head"].as_u64() {
                payload["source_head"] = serde_json::json!(mapping.get(&source));
            }
        }
        "image_version" => identity(&mut payload["entry_id"], mapping),
        "model_request" | "model_request_revision" => {
            if let Some(object) = payload.as_object_mut() {
                object.remove("prompt_cache");
            }
            if let Some(value) = payload.get_mut("context_edits") {
                sequences(value, mapping);
            }
            if public_only
                && let Some(items) = payload
                    .get_mut("input")
                    .and_then(|input| input.get_mut("items"))
                    .and_then(serde_json::Value::as_array_mut)
            {
                items.retain(|item| item["type"] != "provider_state");
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn request(items: serde_json::Value) -> ModelInput {
        serde_json::from_value(json!({ "items": items, "tools": [] })).unwrap()
    }
    #[test]
    fn incomplete_and_reordered_tool_groups_are_rejected() {
        let call =
            json!({ "type": "tool_call", "call_id": "c", "name": "read", "arguments": "{}" });
        let result = json!({
            "type": "tool_result",
            "call_id": "c",
            "result": { "text": "done", "exit_code": null, "truncated": false, "error": null },
        });
        assert!(validate(&request(json!([call.clone()]))).is_err());
        assert!(validate(&request(json!([result.clone(), call.clone()]))).is_err());
        assert!(
            validate(&request(json!([
                call.clone(),
                result.clone(),
                result.clone()
            ])))
            .is_err()
        );
        assert!(validate(&request(json!([call, result]))).is_ok());
    }
    #[test]
    fn copying_legacy_checkpoints_does_not_invent_new_payload_fields() {
        let mut payload = json!({ "summary": "old summary", "first_kept": 0 });
        let original = payload.clone();
        remap_record("compaction", &mut payload, &Default::default(), false);
        assert_eq!(payload, original);
        let mut request = json!({ "request_id": "legacy" });
        remap_record("model_request", &mut request, &Default::default(), true);
        assert_eq!(request, json!({ "request_id": "legacy" }));
    }
    #[test]
    fn copies_rebind_edit_and_image_origins_and_invalidate_comparison_cache() {
        let mapping = std::collections::BTreeMap::from([(8, 3), (9, 4)]);
        let mut edit = json!({
            "base": ["record:8:0"],
            "unchanged": ["record:8:0"],
            "replacement": [{
                "id": "record:8:0",
                "item": { "type": "message", "role": "user", "content": [] },
            }],
        });
        remap_record("context_edit", &mut edit, &mapping, false);
        assert_eq!(edit["replacement"][0]["id"], "record:3:0");
        let mut image = json!({ "entry_id": "record:8:0", "image": {} });
        remap_record("image_version", &mut image, &mapping, false);
        assert_eq!(image["entry_id"], "record:3:0");
        let mut request = json!({ "context_edits": [9], "prompt_cache": { "version": 8 } });
        remap_record("model_request", &mut request, &mapping, false);
        assert_eq!(request["context_edits"], json!([4]));
        assert!(request.get("prompt_cache").is_none());
    }
}
