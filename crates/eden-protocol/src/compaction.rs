//! Replaceable preparation of a durable context checkpoint. Policies generate plans;
//! the context coordinator validates and commits them against the captured store head.
use crate::coding::{ContextInput, ExtensionState};
use serde::{Deserialize, Serialize};

/// Selected policy, independent of the loop, provider, context projection and store roles.
pub const POLICY: &str = "eden.compaction-policy.v1";

/// The trigger is preserved so policies may explicitly reject unsupported operations.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Reason {
    Manual,
    Threshold,
    Overflow,
    PostResponse,
    BranchSummary,
}

/// Preparation has no authority to commit history. `max_cut` protects undelivered
/// queue input; `suggested_cut` is the default recent-context retention boundary.
/// Both cuts count records in `history::active_path(input.records)`, except
/// BranchSummary uses `input.records` itself. They never index `projected` items
/// or identify a global sequence number. `projected` is the current model view.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Request {
    /// Correlates provider attempt events with the eventual durable checkpoint.
    pub request_id: String,
    pub input: ContextInput,
    pub projected: Vec<crate::coding::Item>,
    /// Effective prefix supplied by the coordinator when structural edits change record layout.
    /// Policies must summarize these items rather than reconstructing the original records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_prefix: Option<Vec<crate::coding::Item>>,
    pub reason: Reason,
    pub suggested_cut: usize,
    pub max_cut: usize,
}

/// A policy may return None when no safe useful checkpoint can be generated.
/// The coordinator owns source identities and writes states plus the checkpoint
/// in a single conditional transaction. Empty summaries are always rejected.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Plan {
    /// Exclusive prefix length in the request's record path: 1..=max_cut.
    /// path.len() replaces the entire source path; zero is invalid (return None
    /// for a no-op). The coordinator retains records at and after this index.
    pub cut: usize,
    pub summary: String,
    pub states: Vec<ExtensionState>,
    pub usage: serde_json::Value,
}
