//! Coding role payloads. Only serialized owned data crosses native boundaries.
use crate::Fault;
use serde::{Deserialize, Serialize};
use serde_json::Value;
/// The agent loop: it owns a run and chooses the order of every other role.
pub const LOOP: &str = "eden.coding-loop.v2";
/// Context projection: publishes the tool schemas and chooses the model view.
pub const CONTEXT: &str = "eden.coding-context.v2";
/// Model access, including transient delta emission.
pub const PROVIDER: &str = "eden.coding-provider.v1";
/// Tool execution.
pub const TOOL: &str = "eden.coding-tool.v1";
/// Session storage.
pub const STORE: &str = "eden.session-store.v2";
/// Non-secret model limits, answered independently of the provider.
pub const MODEL_INFO: &str = "eden.model-info.v1";
/// Interpretation of extension state a session needs in order to continue.
pub const INTERPRETER: &str = "eden.record-interpreter.v1";
/// Explicit conversion of extension state between versions.
pub const MIGRATOR: &str = "eden.state-migrator.v1";
/// Pending-submission queue.
pub const QUEUE: &str = "eden.submission-queue.v2";
/// One piece of run content, carrying its data inline so a stored record never
/// depends on the original file still existing.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub enum Block {
    Text {
        text: String,
    },
    Image {
        /// The image's media type, such as `image/png`.
        media_type: String,
        /// The image bytes as base64, so the block stays self-contained.
        data: String,
    },
    File {
        name: String,
        /// The file's media type, such as `application/pdf`.
        media_type: String,
        /// The file bytes as base64, so the block stays self-contained.
        data: String,
    },
}
/// One ordered element of a conversation or model request: a message, a tool
/// call the model asked for, its result, or provider reasoning state.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub enum Item {
    Message {
        role: String,
        content: Vec<Block>,
    },
    ToolCall {
        call_id: String,
        name: String,
        /// The model's own argument text, kept verbatim for the tool to parse.
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
/// What the loop asks the agent-loop role to run: the submitted content, the
/// resolved cwd, and whether this continues a session instead of adding a turn.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunInput {
    /// Continue the session from its committed context instead of adding a turn.
    #[serde(default)]
    pub resume: bool,
    // Its name and type are the whole meaning; see docs/development-checks.md#doc-comments.
    #[allow(missing_docs)]
    pub cwd: String,
    // Its name and type are the whole meaning; see docs/development-checks.md#doc-comments.
    #[allow(missing_docs)]
    pub content: Vec<Block>,
}
/// The context projection request. `action` selects normal projection,
/// compaction or branch summarization; an empty `action` means normal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextInput {
    /// The loaded resources. `None` means the loop has none to offer, not an empty set.
    #[serde(default)]
    pub resources: Option<crate::resources::Snapshot>,
    /// The tool schemas to advertise. `None` asks the context role to publish its own.
    #[serde(default)]
    pub tools: Option<Vec<ToolDefinition>>,
    /// `""`, `"compact"` or `"summarize"`; an unknown value is a producer bug.
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    // Its name and type are the whole meaning; see docs/development-checks.md#doc-comments.
    #[allow(missing_docs)]
    pub records: Vec<Record>,
    /// Extra direction for this projection, such as compaction instructions.
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    // Its name and type are the whole meaning; see docs/development-checks.md#doc-comments.
    #[allow(missing_docs)]
    pub limits: ModelLimits,
    // Its name and type are the whole meaning; see docs/development-checks.md#doc-comments.
    #[allow(missing_docs)]
    pub cwd: String,
    // Its name and type are the whole meaning; see docs/development-checks.md#doc-comments.
    #[allow(missing_docs)]
    pub items: Vec<Item>,
}
/// One tool advertised to the model, with the JSON Schema it accepts.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// The JSON Schema the model's arguments have to satisfy.
    pub parameters: Value,
}
/// The model request. `max_output_tokens` narrows this one request without
/// changing the ordinary allowance, which is what a summary uses.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct ModelInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// A narrower allowance for this one request; a summary uses it.
    pub max_output_tokens: Option<u32>,
    pub items: Vec<Item>,
    pub tools: Vec<ToolDefinition>,
}
/// What the provider returned: the completed items and its own accounting.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct ModelReply {
    pub items: Vec<Item>,
    /// The provider's own accounting, in whatever shape that provider reports.
    #[serde(default)]
    pub usage: Value,
}
/// One dispatch to the tool role, resolved against the session cwd.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct ToolRequest {
    /// The directory a relative path in the arguments resolves against.
    pub cwd: String,
    /// The call this answers, which the loop matches to its own intention.
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}
/// What a tool returned. `truncated` states that the bound cut the output, and
/// a failure is carried as `error` rather than as a missing exit code.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    // Its name and type are the whole meaning; see docs/development-checks.md#doc-comments.
    #[allow(missing_docs)]
    pub text: String,
    /// Present for a command; absent means the tool has no exit status to report.
    pub exit_code: Option<i32>,
    // Its name and type are the whole meaning; see docs/development-checks.md#doc-comments.
    #[allow(missing_docs)]
    pub truncated: bool,
    // Its name and type are the whole meaning; see docs/development-checks.md#doc-comments.
    #[allow(missing_docs)]
    pub error: Option<Fault>,
}
/// One public history node: its identity, its place in the branch, and the
/// payload of its kind. A record is written once and never rewritten.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    /// The node this one descends from; absent only on a session's first node.
    #[serde(default)]
    pub parent_id: Option<u64>,
    /// The branch this node was committed on, which selection records can change.
    #[serde(default = "main_branch")]
    pub branch: String,
    // Its name and type are the whole meaning; see docs/development-checks.md#doc-comments.
    #[allow(missing_docs)]
    pub schema_version: u32,
    // Its name and type are the whole meaning; see docs/development-checks.md#doc-comments.
    #[allow(missing_docs)]
    pub session_id: u64,
    // Its name and type are the whole meaning; see docs/development-checks.md#doc-comments.
    #[allow(missing_docs)]
    pub sequence: u64,
    // Its name and type are the whole meaning; see docs/development-checks.md#doc-comments.
    #[allow(missing_docs)]
    pub run_id: u64,
    // Its name and type are the whole meaning; see docs/development-checks.md#doc-comments.
    #[allow(missing_docs)]
    pub kind: String,
    // Its name and type are the whole meaning; see docs/development-checks.md#doc-comments.
    #[allow(missing_docs)]
    pub payload: Value,
}
/// One storage operation, sent by the loop and answered with exactly one [`StoreReply`].
///
/// An [`AppendBatch`](StoreRequest::AppendBatch) is committed as a unit so a
/// response and its tool intentions cannot be acknowledged apart,
/// [`Navigate`](StoreRequest::Navigate) keeps every prior node, and
/// [`Create`](StoreRequest::Create) refuses an existing destination.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub enum StoreRequest {
    RestoreMemory {
        session_id: u64,
        records: Vec<Record>,
    },
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
/// The store's answer. Its receipt is the reply itself: a returned `sequence`
/// states that the request is durably committed.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct StoreReply {
    #[serde(default)]
    pub active_head: Option<u64>,
    #[serde(default = "main_branch")]
    pub active_branch: String,
    pub session_id: u64,
    pub sequence: u64,
    pub records: Vec<Record>,
}
/// One pending submission: a stable identity, the branch it belongs to, and
/// whether delivery is steering or follow-up.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
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
/// One queue operation. Acceptance, delivery and consumption are separate
/// records, so a consumption that never commits leaves the entry pending
/// instead of requeueing work whose tools may already have run.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
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
/// A record the store has not identified yet: its sequence, run and branch
/// belong to the batch that carries it, not to the author of the entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct RecordDraft {
    pub kind: String,
    pub payload: Value,
}
/// The limits a model reports. A zero `context_window` means unknown, which
/// disables threshold detection for automatic compaction.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct ModelLimits {
    pub context_window: u64,
    pub max_output_tokens: u32,
}
/// An extension state declares whether it is needed to continue this branch.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct ExtensionState {
    pub namespace: String,
    pub version: u32,
    pub required: bool,
    pub summary: String,
    pub value: Value,
}
/// Asks an interpreter to project the extension state it understands into
/// conversation items.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct InterpretRequest {
    pub states: Vec<ExtensionState>,
}
/// The interpreted state. Only current required state has to be understood for
/// a session to continue; display-only state may be ignored.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct InterpretReply {
    pub items: Vec<Item>,
}
/// Asks a migrator to convert state between versions. Without `apply` the
/// request is a preview, and an applied result has to match the preview the
/// caller already reviewed.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct MigrateRequest {
    pub states: Vec<ExtensionState>,
    pub apply: bool,
}
/// The converted state, with one entry per source record in its original
/// order, plus what the conversion preserved and what it lost.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct MigrateReply {
    pub states: Vec<ExtensionState>,
    pub preserved: Vec<String>,
    pub losses: Vec<String>,
}
