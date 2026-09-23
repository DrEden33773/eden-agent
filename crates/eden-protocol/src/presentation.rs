//! Portable live presentation data. The same value is consumed by terminal and browser adapters.
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    Ordinary,
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
