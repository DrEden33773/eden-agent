//! Coding role payloads. Only serialized owned data crosses native boundaries.
use crate::Fault;
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub const LOOP: &str = "eden.coding-loop.v2";
pub const CONTEXT: &str = "eden.coding-context.v2";
pub const PROVIDER: &str = "eden.coding-provider.v1";
pub const TOOL: &str = "eden.coding-tool.v1";
pub const STORE: &str = "eden.session-store.v2";
pub const MODEL_INFO: &str = "eden.model-info.v1";
pub const INTERPRETER: &str = "eden.record-interpreter.v1";
pub const MIGRATOR: &str = "eden.state-migrator.v1";
pub const QUEUE: &str = "eden.submission-queue.v2";
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Image {
        media_type: String,
        data: String,
    },
    File {
        name: String,
        media_type: String,
        data: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    Message {
        role: String,
        content: Vec<Block>,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        call_id: String,
        result: ToolResult,
    },
    /// Opaque provider reasoning state, retained for subsequent requests to that provider.
    ProviderState {
        provider: String,
        value: Value,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunInput {
    #[serde(default)]
    pub resume: bool,
    pub cwd: String,
    pub content: Vec<Block>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextInput {
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub records: Vec<Record>,
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub limits: ModelLimits,
    pub cwd: String,
    pub items: Vec<Item>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    pub items: Vec<Item>,
    pub tools: Vec<ToolDefinition>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelReply {
    pub items: Vec<Item>,
    #[serde(default)]
    pub usage: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolRequest {
    pub cwd: String,
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub text: String,
    pub exit_code: Option<i32>,
    pub truncated: bool,
    pub error: Option<Fault>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    #[serde(default)]
    pub parent_id: Option<u64>,
    #[serde(default = "main_branch")]
    pub branch: String,
    pub schema_version: u32,
    pub session_id: u64,
    pub sequence: u64,
    pub run_id: u64,
    pub kind: String,
    pub payload: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum StoreRequest {
    Open {
        path: Option<String>,
        session_id: u64,
    },
    Append {
        run_id: u64,
        kind: String,
        payload: Value,
    },
    AppendBatch {
        run_id: u64,
        entries: Vec<RecordDraft>,
    },
    Navigate {
        target: u64,
        branch: String,
    },
    Create {
        path: String,
        session_id: u64,
        records: Vec<Record>,
    },
    Read,
    Close,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreReply {
    #[serde(default)]
    pub active_head: Option<u64>,
    #[serde(default = "main_branch")]
    pub active_branch: String,
    pub session_id: u64,
    pub sequence: u64,
    pub records: Vec<Record>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueEntry {
    #[serde(default = "main_branch")]
    pub branch: String,
    pub id: u64,
    pub kind: String,
    pub content: Vec<Block>,
}
/// Validate complete public JSONL records without loading a storage or business plugin.
/// An interrupted tail is diagnosed and preserved; reading never repairs or replays it.
pub fn decode_records(bytes: &[u8]) -> Result<Vec<Record>, Fault> {
    let scan = crate::history::scan_records(bytes);
    if let Some(message) = scan.diagnostic {
        return Err(Fault::new("PersistenceFailure", "public-history", message));
    }
    Ok(scan.records)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum QueueRequest {
    Enqueue { kind: String, content: Vec<Block> },
    Take { kind: String },
    Inspect,
    Configure { steering: String, follow_up: String },
    Consume { ids: Vec<u64> },
    Restore,
}

fn main_branch() -> String {
    "main".into()
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordDraft {
    pub kind: String,
    pub payload: Value,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelLimits {
    pub context_window: u64,
    pub max_output_tokens: u32,
}
/// An extension state declares whether it is needed to continue this branch.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExtensionState {
    pub namespace: String,
    pub version: u32,
    pub required: bool,
    pub summary: String,
    pub value: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InterpretRequest {
    pub states: Vec<ExtensionState>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InterpretReply {
    pub items: Vec<Item>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrateRequest {
    pub states: Vec<ExtensionState>,
    pub apply: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrateReply {
    pub states: Vec<ExtensionState>,
    pub preserved: Vec<String>,
    pub losses: Vec<String>,
}
