//! Bounded, read-only access to original payloads on the current history branch.
use serde::{Deserialize, Serialize};

/// Shared by notes and independent tools; never executes historical tool calls.
pub const RECALL: &str = "eden.history-recall.v1";
/// Contribution catalog consumed with the ordinary tool-catalog payloads.
pub const TOOLS: &str = "eden.history-recall-tools.v1";
/// Contribution execution route consumed with ordinary coding-tool payloads.
pub const TOOL: &str = "eden.history-recall-tool.v1";

/// Search is case-sensitive literal substring matching over compact payload JSON.
/// Bounds are inclusive session-local record sequences, restricted to ancestors
/// of the active head. Read never accesses a sibling branch's record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
#[allow(missing_docs)]
pub enum RecallQuery {
    Search {
        literal: String,
        #[serde(default)]
        from_sequence: Option<u64>,
        #[serde(default)]
        through_sequence: Option<u64>,
    },
    Read {
        sequence: u64,
    },
}

/// A page request. Byte limits count returned payload text, excluding provenance
/// and JSON framing; they must be 4..=65536, and record limits 1..=128.
/// Cursors are opaque and continue the same query against the original snapshot.
/// Pure appends on that branch are allowed; navigation requires a new query.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(missing_docs)]
pub struct RecallRequest {
    pub query: RecallQuery,
    #[serde(default = "default_bytes")]
    pub max_bytes: usize,
    #[serde(default = "default_records")]
    pub max_records: usize,
    #[serde(default)]
    pub cursor: Option<String>,
}
fn default_bytes() -> usize {
    8192
}
fn default_records() -> usize {
    16
}

/// Stable within the source session, including after compaction. A copied
/// session has a different identity; a record's origin branch may be an ancestor
/// of the selected branch. Text is a UTF-8 slice of compact original payload JSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct RecallChunk {
    pub session_id: u64,
    pub sequence: u64,
    pub branch: String,
    pub run_id: u64,
    pub kind: String,
    pub byte_start: usize,
    pub byte_end: usize,
    pub total_bytes: usize,
    pub text: String,
}

/// Empty matches with `truncated=false` means exhaustion, not an omitted page.
/// When truncated, `next_cursor` resumes without losing a clipped record's tail.
/// Search pages are in ascending sequence order. The snapshot identity also
/// allows callers to attribute an empty result to the branch that was searched.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct RecallReply {
    pub session_id: u64,
    pub active_branch: String,
    pub active_head: Option<u64>,
    pub revision: u64,
    pub chunks: Vec<RecallChunk>,
    pub truncated: bool,
    pub next_cursor: Option<String>,
}
