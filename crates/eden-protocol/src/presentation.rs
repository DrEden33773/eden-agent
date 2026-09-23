//! Portable live presentation data. The same value is consumed by terminal and browser adapters.
use crate::{coding::Record, delivery::Selection, history};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

/// Host service used by author contexts to publish or remove a view.
pub const HOST: &str = "eden.presentation.host.v1";
/// Package-local action contract. The host addresses the owner package, never a global role winner.
pub const ACTION: &str = "eden.presentation.action.v1";
/// Version of the semantic node vocabulary.
pub const VERSION: u32 = 1;

/// Where a live contribution appears. Slot order is stable across frontends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Slot {
    ToolResult,
    Header,
    Footer,
    Panel,
    Overlay,
    Composer,
}

/// A source reference and its visibility class for a later static projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Source {
    pub record_sequence: Option<u64>,
    pub class: ContentClass,
}
/// Selection class; a static exporter must drop the whole derived view when its source is excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum ContentClass {
    Message,
    Tool,
    Thinking,
    FullOutput,
    Attachment,
    Extension,
}

/// A platform-independent node. IDs let renderers retain local drafts and scroll positions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Node {
    Text {
        id: String,
        text: String,
    },
    Code {
        id: String,
        language: Option<String>,
        text: String,
    },
    Diff {
        id: String,
        before: String,
        after: String,
    },
    Table {
        id: String,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Group {
        id: String,
        title: String,
        children: Vec<Node>,
    },
    Attachment {
        id: String,
        name: String,
        record_sequence: u64,
    },
    Status {
        id: String,
        text: String,
    },
    Form {
        id: String,
        action: String,
        fields: Vec<Field>,
    },
    Button {
        id: String,
        action: String,
        label: String,
    },
}
impl Node {
    /// Stable identity within one view revision series.
    pub fn id(&self) -> &str {
        match self {
            Self::Text { id, .. }
            | Self::Code { id, .. }
            | Self::Diff { id, .. }
            | Self::Table { id, .. }
            | Self::Group { id, .. }
            | Self::Attachment { id, .. }
            | Self::Status { id, .. }
            | Self::Form { id, .. }
            | Self::Button { id, .. } => id,
        }
    }
}
/// Form input type, excluding secret/authentication values from the public presentation channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum FieldKind {
    Text,
    Choice,
    MultiChoice,
    Boolean,
}
/// One validated input field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Field {
    pub id: String,
    pub label: String,
    pub kind: FieldKind,
    pub required: bool,
    #[serde(default)]
    pub initial: Option<Value>,
    #[serde(default)]
    pub options: Vec<String>,
}
impl Field {
    /// Required or optional plain text field.
    pub fn text(id: impl Into<String>, label: impl Into<String>, required: bool) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind: FieldKind::Text,
            required,
            initial: None,
            options: vec![],
        }
    }
    /// Offer one selection from a fixed list, validated by the host before dispatch.
    pub fn choice(
        id: impl Into<String>,
        label: impl Into<String>,
        options: Vec<String>,
        required: bool,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind: FieldKind::Choice,
            required,
            initial: None,
            options,
        }
    }
    /// Offer several selections from a fixed list, validated by the host before dispatch.
    pub fn multi_choice(
        id: impl Into<String>,
        label: impl Into<String>,
        options: Vec<String>,
        required: bool,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind: FieldKind::MultiChoice,
            required,
            initial: None,
            options,
        }
    }
    /// Capture a true/false choice without treating it as secret input.
    pub fn boolean(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind: FieldKind::Boolean,
            required: true,
            initial: None,
            options: vec![],
        }
    }
    /// Supply a portable starting value; the frontend retains later edits locally.
    pub fn initial(mut self, value: Value) -> Self {
        self.initial = Some(value);
        self
    }
}
/// An author-owned semantic description before the host assigns a revision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct View {
    pub id: String,
    pub slot: Slot,
    pub title: String,
    pub fallback: String,
    pub source: Option<Source>,
    pub nodes: Vec<Node>,
    /// A named platform-specific contribution must supply a portable fallback.
    pub platforms: Vec<String>,
}
impl View {
    /// Start one portable view with a plain fallback equal to its title.
    pub fn new(id: impl Into<String>, slot: Slot, title: impl Into<String>) -> Self {
        let title = title.into();
        Self {
            id: id.into(),
            slot,
            fallback: title.clone(),
            title,
            source: None,
            nodes: vec![],
            platforms: vec![],
        }
    }
    /// Append a semantic node.
    pub fn node(mut self, node: Node) -> Self {
        self.nodes.push(node);
        self
    }
    /// Carry the originating record and visibility class into SP-B's later static projection.
    pub fn source(mut self, source: Source) -> Self {
        self.source = Some(source);
        self
    }
    /// Keep an accessible plain reading path for unsupported platforms or vocabulary versions.
    pub fn fallback(mut self, text: impl Into<String>) -> Self {
        self.fallback = text.into();
        self
    }
    /// Mark a contribution as platform-specific; default views are portable to both adapters.
    pub fn platforms(mut self, platforms: Vec<String>) -> Self {
        self.platforms = platforms;
        self
    }
}
/// One host-assigned revision of a contributed view.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct LiveView {
    pub owner: String,
    pub run_id: u64,
    pub revision: u64,
    pub active: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handled_actions: Vec<String>,
    #[serde(flatten)]
    pub view: View,
}
/// Host request sent through the serialized SDK service bridge.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum HostRequest {
    Publish { owner: String, view: View },
    Remove { owner: String, id: String },
}
/// Revision issued by the host when a view is published.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Revision {
    pub value: u64,
}
/// A user action admitted against one exact live view revision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct ActionRequest {
    #[serde(with = "session_identity")]
    pub session_id: u64,
    pub owner: String,
    pub view_id: String,
    pub revision: u64,
    pub action: String,
    pub request_id: String,
    #[serde(default)]
    pub values: Value,
}
/// One frontend-local input focus that is currently changing, with no draft content.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum ActivityTarget {
    Composer,
    Form {
        owner: String,
        view_id: String,
        node_id: String,
    },
}
/// Transient activity from one attachment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Activity {
    pub attachment: u64,
    pub frontend: String,
    pub target: ActivityTarget,
}
/// Atomic state and its sequence, suitable for late attach or a lagged consumer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Snapshot {
    pub version: u32,
    #[serde(with = "session_identity")]
    pub session_id: u64,
    pub sequence: u64,
    pub views: Vec<LiveView>,
    pub activity: Vec<Activity>,
    pub pending_interactions: Vec<Value>,
}

/// A portable terminal snapshot written after a run's terminal record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StaticRecord {
    /// Node vocabulary version used when these views were saved.
    pub version: u32,
    /// Complete raw views so a newer writer does not make old readers fail.
    pub views: Vec<Value>,
}

/// Read settled views from the selected ancestry without loading author packages.
pub fn static_views(records: &[Record], selection: &Selection) -> Vec<LiveView> {
    if history::validate_records(records).is_err() {
        return vec![];
    }
    let mut head = selection.head.or_else(|| {
        history::branch_state(records)
            .ok()
            .and_then(|state| state.0)
    });
    let mut path = vec![];
    while let Some(sequence) = head {
        let Some(record) = records
            .get((sequence.saturating_sub(1)) as usize)
            .filter(|record| record.sequence == sequence && record.kind != "branch_selected")
        else {
            return vec![];
        };
        path.push(record);
        head = record.parent_id;
    }
    path.reverse();
    let attachments: BTreeSet<u64> = path
        .iter()
        .filter(|record| {
            record.kind == "attachment" || contains_embedded_attachment(&record.payload)
        })
        .map(|record| record.sequence)
        .collect();
    let mut views = vec![];
    for record in path
        .iter()
        .filter(|record| record.kind == "presentation_static")
    {
        if !selection.runs.is_empty() && !selection.runs.contains(&record.run_id) {
            continue;
        }
        let Ok(saved) = serde_json::from_value::<StaticRecord>(record.payload.clone()) else {
            continue;
        };
        for (index, raw) in saved.views.into_iter().enumerate() {
            let Some(source) = raw
                .get("source")
                .and_then(|value| serde_json::from_value::<Source>(value.clone()).ok())
            else {
                continue;
            };
            let Some(sequence) = source.record_sequence else {
                continue;
            };
            let Some(origin) = path.iter().find(|candidate| candidate.sequence == sequence) else {
                continue;
            };
            let run_id = raw.get("run_id").and_then(Value::as_u64);
            if !class_selected(source.class, selection)
                || run_id != Some(record.run_id)
                || run_id != Some(origin.run_id)
                || !source_record_matches(source.class, origin)
            {
                continue;
            }
            let Some(fallback) = raw.get("fallback").and_then(Value::as_str) else {
                continue;
            };
            let had_attachment = raw
                .get("nodes")
                .and_then(Value::as_array)
                .is_some_and(|nodes| nodes.iter().any(contains_attachment));
            let hide_attachment_text =
                !selection.attachments && (had_attachment || saved.version != VERSION);
            let title = if hide_attachment_text {
                "Saved presentation"
            } else {
                raw.get("title").and_then(Value::as_str).unwrap_or(fallback)
            };
            let fallback = if hide_attachment_text {
                "Saved content"
            } else {
                fallback
            };
            let recognized_slot = raw
                .get("slot")
                .and_then(|value| serde_json::from_value::<Slot>(value.clone()).ok());
            let mut nodes = if saved.version == VERSION && recognized_slot.is_some() {
                raw.get("nodes")
                    .and_then(Value::as_array)
                    .map(|nodes| {
                        nodes
                            .iter()
                            .filter_map(|node| static_node(node, &attachments, selection))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            } else {
                vec![]
            };
            if nodes.is_empty() {
                nodes.push(Node::Text {
                    id: "static-fallback".into(),
                    text: fallback.into(),
                });
            }
            let slot = raw
                .get("slot")
                .and_then(|value| serde_json::from_value::<Slot>(value.clone()).ok())
                .unwrap_or(Slot::Panel);
            let platforms = raw
                .get("platforms")
                .and_then(|value| serde_json::from_value::<Vec<String>>(value.clone()).ok())
                .unwrap_or_default();
            let view = LiveView {
                owner: raw
                    .get("owner")
                    .and_then(Value::as_str)
                    .unwrap_or("saved")
                    .into(),
                run_id: record.run_id,
                revision: raw.get("revision").and_then(Value::as_u64).unwrap_or(0),
                active: false,
                handled_actions: vec![],
                view: View {
                    id: raw.get("id").and_then(Value::as_str).map_or_else(
                        || format!("saved-{}-{index}", record.sequence),
                        str::to_owned,
                    ),
                    slot,
                    title: title.into(),
                    fallback: fallback.into(),
                    source: Some(source),
                    nodes,
                    platforms,
                },
            };
            views.push(view);
        }
    }
    views
}

/// Rebase static source and attachment references when a history copy renumbers records.
pub fn remap_static_record(payload: &mut Value, mapping: &std::collections::BTreeMap<u64, u64>) {
    let Some(views) = payload.get_mut("views").and_then(Value::as_array_mut) else {
        return;
    };
    views.retain_mut(|view| {
        let Some(source) = view.get_mut("source") else {
            return false;
        };
        let Some(old) = source.get("record_sequence").and_then(Value::as_u64) else {
            return false;
        };
        let Some(new) = mapping.get(&old) else {
            return false;
        };
        source["record_sequence"] = serde_json::json!(new);
        if let Some(nodes) = view.get_mut("nodes").and_then(Value::as_array_mut) {
            remap_nodes(nodes, mapping);
        }
        true
    });
}
fn remap_nodes(nodes: &mut Vec<Value>, mapping: &std::collections::BTreeMap<u64, u64>) {
    nodes.retain_mut(|node| match node.get("kind").and_then(Value::as_str) {
        Some("attachment") => {
            let Some(old) = node.get("record_sequence").and_then(Value::as_u64) else {
                return false;
            };
            let Some(new) = mapping.get(&old) else {
                return false;
            };
            node["record_sequence"] = serde_json::json!(new);
            true
        }
        Some("group") => {
            if let Some(children) = node.get_mut("children").and_then(Value::as_array_mut) {
                remap_nodes(children, mapping);
            }
            true
        }
        Some("text" | "code" | "diff" | "table" | "status" | "form" | "button") => true,
        _ => false,
    });
}

/// Whether a committed record can supply the declared static selection class.
pub fn source_record_matches(class: ContentClass, record: &Record) -> bool {
    match class {
        ContentClass::Message => matches!(record.kind.as_str(), "message" | "queue_delivered"),
        ContentClass::Tool => matches!(
            record.kind.as_str(),
            "tool_intent" | "tool_result" | "terminal"
        ),
        ContentClass::Thinking => record.kind == "provider_state",
        ContentClass::FullOutput => record.kind == "tool_result",
        ContentClass::Attachment => {
            record.kind == "attachment" || contains_embedded_attachment(&record.payload)
        }
        ContentClass::Extension => record.kind == "terminal" || record.kind.contains('.'),
    }
}
/// Selection class shared by static reading and JSONL export.
pub fn class_selected(class: ContentClass, selection: &Selection) -> bool {
    match class {
        ContentClass::Message => selection.messages,
        ContentClass::Tool => selection.tools,
        ContentClass::Thinking => selection.thinking,
        ContentClass::FullOutput => selection.full_outputs,
        ContentClass::Attachment => selection.attachments,
        ContentClass::Extension => selection.extensions,
    }
}
fn contains_embedded_attachment(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "file" | "image"))
                || object.iter().any(|(key, child)| {
                    key == "artifacts" && child.as_array().is_some_and(|items| !items.is_empty())
                        || contains_embedded_attachment(child)
                })
        }
        Value::Array(items) => items.iter().any(contains_embedded_attachment),
        _ => false,
    }
}
fn contains_attachment(node: &Value) -> bool {
    node.get("kind").and_then(Value::as_str) == Some("attachment")
        || node
            .get("children")
            .and_then(Value::as_array)
            .is_some_and(|children| children.iter().any(contains_attachment))
}
fn static_node(raw: &Value, attachments: &BTreeSet<u64>, selection: &Selection) -> Option<Node> {
    if raw.get("kind").and_then(Value::as_str) == Some("group") {
        let children = raw
            .get("children")?
            .as_array()?
            .iter()
            .filter_map(|child| static_node(child, attachments, selection))
            .collect();
        return Some(Node::Group {
            id: raw.get("id")?.as_str()?.into(),
            title: raw.get("title")?.as_str()?.into(),
            children,
        });
    }
    let node: Node = serde_json::from_value(raw.clone()).ok()?;
    match node {
        Node::Attachment {
            record_sequence, ..
        } if !selection.attachments || !attachments.contains(&record_sequence) => None,
        Node::Form { .. } | Node::Button { .. } => None,
        _ => Some(node),
    }
}

/// JSON consumers such as browsers cannot represent an arbitrary u64 as a number.
mod session_identity {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Identity {
            Text(String),
            Number(u64),
        }
        match Identity::deserialize(deserializer)? {
            Identity::Text(text) => text.parse().map_err(serde::de::Error::custom),
            Identity::Number(number) => Ok(number),
        }
    }
}

#[cfg(test)]
mod static_tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn record(sequence: u64, kind: &str, payload: Value) -> Record {
        Record {
            schema_version: 2,
            session_id: 7,
            sequence,
            run_id: 1,
            parent_id: (sequence > 1).then_some(sequence - 1),
            branch: "main".into(),
            kind: kind.into(),
            payload,
        }
    }
    fn history() -> Vec<Record> {
        let view = LiveView {
            owner: "removed-plugin".into(),
            run_id: 1,
            revision: 9,
            active: false,
            handled_actions: vec![],
            view: View::new("result", Slot::ToolResult, "Attachment review.pdf")
                .fallback("Attachment review.pdf")
                .source(Source {
                    record_sequence: Some(3),
                    class: ContentClass::Tool,
                })
                .node(Node::Diff {
                    id: "diff".into(),
                    before: "old".into(),
                    after: "new".into(),
                })
                .node(Node::Attachment {
                    id: "file".into(),
                    name: "review.pdf".into(),
                    record_sequence: 2,
                })
                .node(Node::Button {
                    id: "retry".into(),
                    action: "retry".into(),
                    label: "Run again".into(),
                }),
        };
        vec![
            record(1, "session", json!({})),
            record(
                2,
                "message",
                json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{
                        "type": "file",
                        "name": "review.pdf",
                        "media_type": "application/pdf",
                        "data": "AA==",
                    }],
                }),
            ),
            record(3, "terminal", json!({ "status": "completed" })),
            record(
                4,
                "presentation_static",
                json!(StaticRecord {
                    version: VERSION,
                    views: vec![json!(view)]
                }),
            ),
        ]
    }
    #[test]
    fn selection_drops_actions_and_attachment_metadata_without_losing_the_diff() {
        let records = history();
        let views = static_views(&records, &Selection::default());
        assert_eq!(views.len(), 1);
        assert!(!views[0].active);
        assert_eq!(views[0].view.title, "Saved presentation");
        assert_eq!(views[0].view.fallback, "Saved content");
        assert!(matches!(
            views[0].view.nodes.as_slice(),
            [Node::Diff { .. }]
        ));
        let selection = Selection {
            tools: false,
            ..Selection::default()
        };
        assert!(static_views(&records, &selection).is_empty());
        let selection = Selection {
            attachments: true,
            ..Selection::default()
        };
        assert!(matches!(
            static_views(&records, &selection)[0].view.nodes.as_slice(),
            [Node::Diff { .. }, Node::Attachment { .. }]
        ));
    }
    #[test]
    fn copy_rebases_source_and_attachment_and_drops_missing_references() {
        let mut payload = history()[3].payload.clone();
        remap_static_record(&mut payload, &BTreeMap::from([(2, 8), (3, 9)]));
        assert_eq!(payload["views"][0]["source"]["record_sequence"], 9);
        assert_eq!(payload["views"][0]["nodes"][1]["record_sequence"], 8);
        remap_static_record(&mut payload, &BTreeMap::from([(9, 5)]));
        assert_eq!(payload["views"][0]["nodes"].as_array().unwrap().len(), 2);
        remap_static_record(&mut payload, &BTreeMap::new());
        assert!(payload["views"].as_array().unwrap().is_empty());
    }
    #[test]
    fn unknown_nodes_keep_known_siblings_and_future_versions_use_fallback() {
        let mut records = history();
        records[3].payload["views"][0]["nodes"] = json!([{
            "kind": "group",
            "id": "group",
            "title": "Known",
            "children": [
                { "kind": "future_chart", "id": "new" },
                { "kind": "text", "id": "old", "text": "Readable" }
            ],
        }]);
        let selection = Selection {
            attachments: true,
            ..Selection::default()
        };
        let views = static_views(&records, &selection);
        assert!(
            matches!(&views[0].view.nodes[0], Node::Group { children, .. }
            if matches!(children.as_slice(), [Node::Text { text, .. }] if text == "Readable"))
        );
        records[3].payload["views"][0]["slot"] = json!("future_slot");
        let views = static_views(&records, &selection);
        assert!(matches!(
            views[0].view.nodes.as_slice(),
            [Node::Text { .. }]
        ));
        records[3].payload["version"] = json!(VERSION + 1);
        let views = static_views(&records, &selection);
        assert!(
            matches!(views[0].view.nodes.as_slice(), [Node::Text { text, .. }]
            if text == "Attachment review.pdf")
        );
    }
}
