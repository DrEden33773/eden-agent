use crate::summary::RunSummary;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    Thinking,
    Tool,
    Notice,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToolState {
    Pending,
    Success,
    Failed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSummary {
    pub title: String,
    pub detail: String,
    pub state: ToolState,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    #[serde(skip)]
    pub images: Vec<std::sync::Arc<eden_protocol::coding::Block>>,
    pub id: u64,
    #[serde(default)]
    pub children: Vec<ToolSummary>,
    #[serde(default)]
    pub summary: Option<RunSummary>,
    pub role: Role,
    pub title: String,
    pub body: String,
    pub expanded: bool,
    pub failed: bool,
    #[serde(default)]
    pub pending: bool,
    pub before: Option<String>,
    pub after: Option<String>,
    pub revision: u64,
}
impl Message {
    pub fn run_summary(id: u64, summary: RunSummary) -> Self {
        let mut message = Self::new(id, Role::Notice, "", summary.formatted().0);
        message.summary = Some(summary);
        message
    }
    pub fn new(id: u64, role: Role, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            images: vec![],
            id,
            children: vec![],
            summary: None,
            role,
            title: title.into(),
            body: body.into(),
            expanded: false,
            failed: false,
            pending: false,
            before: None,
            after: None,
            revision: 0,
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attachment {
    pub name: String,
    pub source: String,
    pub bytes: std::sync::Arc<[u8]>,
    #[serde(default)]
    pub media_type: Option<String>,
    pub image: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Draft {
    #[serde(default)]
    pub references: Vec<std::sync::Arc<eden_protocol::session_reference::Reference>>,
    pub text: String,
    pub cursor: usize,
    pub attachments: Vec<Attachment>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ViewPosition {
    pub anchor: Anchor,
    pub follow: bool,
    pub selected_record: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedDraft {
    #[serde(default)]
    pub positions: BTreeMap<String, ViewPosition>,
    #[serde(default)]
    pub sessions: BTreeMap<String, Draft>,
    pub version: u32,
    pub session: String,
    pub frontend: String,
    pub draft: Draft,
    #[serde(default)]
    pub pending: Option<PendingSubmission>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingSubmission {
    pub route: String,
    pub body: serde_json::Value,
    pub draft: Draft,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Editor,
    Transcript,
    Inspector,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Running,
    Waiting,
    Cancelling,
    Failed,
}
impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Ready",
            Self::Running => "Working",
            Self::Waiting => "Waiting for response",
            Self::Cancelling => "Cleaning up",
            Self::Failed => "Run failed",
        }
    }
}
#[derive(Clone, Debug)]
pub struct FieldPicker {
    pub query: String,
    pub selected: usize,
}
#[derive(Clone, Debug)]
pub struct Field {
    pub key: String,
    pub kind: String,
    pub initial: String,
    pub clear: bool,
    pub inherit: bool,
    pub options: Vec<String>,
    pub option_values: Vec<serde_json::Value>,
    pub label: String,
    pub value: String,
    pub private: bool,
    pub readonly: bool,
    pub required: bool,
    pub allow_clear: bool,
    pub minimum: Option<i64>,
    pub maximum: Option<i64>,
    pub cursor: usize,
    pub picker: Option<FieldPicker>,
}
impl Field {
    pub fn text(label: &str, value: &str) -> Self {
        Self {
            key: label.into(),
            kind: "text".into(),
            initial: value.into(),
            clear: false,
            inherit: false,
            options: vec![],
            option_values: vec![],
            label: label.into(),
            value: value.into(),
            private: false,
            readonly: false,
            required: false,
            allow_clear: false,
            minimum: None,
            maximum: None,
            cursor: value.len(),
            picker: None,
        }
    }
}
#[derive(Clone, Debug)]
pub enum Dialog {
    Details {
        title: String,
        text: String,
    },
    Palette {
        kind: String,
        query: String,
        selected: usize,
    },
    Form {
        title: String,
        fields: Vec<Field>,
        selected: usize,
        status: String,
    },
    Settings {
        selected: usize,
    },
    Help,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    pub colors: BTreeMap<String, u32>,
    pub bindings: BTreeMap<String, String>,
    pub light: bool,
    pub compact: bool,
    pub basic: bool,
    pub inspector: bool,
    pub navigator: bool,
    pub thinking: bool,
    pub footer: bool,
    pub diff_split: bool,
    pub mouse: bool,
    pub motion: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            colors: BTreeMap::new(),
            bindings: BTreeMap::new(),
            light: false,
            compact: false,
            basic: false,
            inspector: false,
            navigator: false,
            thinking: true,
            footer: true,
            diff_split: true,
            mouse: true,
            motion: true,
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Anchor {
    pub record: u64,
    pub line: usize,
    pub byte: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextRole {
    ToolName,
    ToolTarget,
    ToolError,
    Heading,
    Underline,
    Strong,
    Emphasis,
    Strike,
    InlineCode,
    Link,
    Quote,
    ListMarker,
    CodeFence,
    CodeKeyword,
    CodeString,
    CodeNumber,
    CodeComment,
    CodeType,
    CodeFunction,
    DiffAdded,
    DiffRemoved,
    DiffGutter,
    Muted,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextSpan {
    pub range: std::ops::Range<usize>,
    pub role: TextRole,
}
#[derive(Clone, Debug)]
pub struct Row {
    pub image: Option<crate::transcript_images::Slice>,
    pub tree_stem: bool,
    pub inset: u16,
    pub summary: bool,
    pub source: Option<String>,
    pub anchor: Anchor,
    pub text: String,
    pub spans: Vec<TextSpan>,
    pub kind: Role,
    pub dim: bool,
    pub failed: bool,
}

impl Preferences {
    pub fn valid(&self) -> bool {
        self.colors.iter().all(|(key, value)| {
            [
                "foreground",
                "muted",
                "accent",
                "success",
                "error",
                "border",
                "heading",
                "key",
                "code",
                "link",
                "quote",
                "number",
                "type",
                "function",
                "dim",
                "warning",
            ]
            .contains(&key.as_str())
                && *value <= 0xffffff
        }) && self.bindings.iter().all(|(key, action)| {
            key.split_once('.').is_some_and(|(context, key)| {
                ["composer", "transcript", "inspector", "modal"].contains(&context)
                    && !key.is_empty()
            }) && [
                "send",
                "cancel",
                "copy",
                "clear",
                "external_editor",
                "quit",
                "search",
                "inspector",
                "navigator",
                "settings",
                "steering",
                "follow_up",
                "paste",
                "undo",
                "redo",
                "close",
            ]
            .contains(&action.as_str())
        })
    }
}
