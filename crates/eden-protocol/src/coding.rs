//! Coding role payloads. Only serialized owned data crosses native boundaries.
use crate::Fault;
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub const LOOP: &str = "eden.coding-loop.v1";
pub const CONTEXT: &str = "eden.coding-context.v1";
pub const PROVIDER: &str = "eden.coding-provider.v1";
pub const TOOL: &str = "eden.coding-tool.v1";
pub const STORE: &str = "eden.session-store.v1";
pub const QUEUE: &str = "eden.submission-queue.v1";
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
    pub cwd: String,
    pub content: Vec<Block>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextInput {
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
    Read,
    Close,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreReply {
    pub session_id: u64,
    pub sequence: u64,
    pub records: Vec<Record>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueEntry {
    pub id: u64,
    pub kind: String,
    pub content: Vec<Block>,
}
/// Validate complete public JSONL records without loading a storage or business plugin.
/// An interrupted tail is diagnosed and preserved; reading never repairs or replays it.
pub fn decode_records(bytes: &[u8]) -> Result<Vec<Record>, Fault> {
    let invalid = |message: &str| Fault::new("PersistenceFailure", "public-history", message);
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(invalid("incomplete history tail; source preserved"));
    }
    let mut records: Vec<Record> = vec![];
    for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let record: Record =
            serde_json::from_slice(line).map_err(|_| invalid("invalid public record"))?;
        if record.schema_version != 1
            || record.sequence != records.len() as u64 + 1
            || records
                .first()
                .is_some_and(|first| first.session_id != record.session_id)
        {
            return Err(invalid(
                "unsupported schema, discontinuous sequence, or mixed identity",
            ));
        }
        records.push(record);
    }
    Ok(records)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum QueueRequest {
    Enqueue { kind: String, content: Vec<Block> },
    Take { kind: String },
    Inspect,
}
