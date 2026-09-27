//! Auxiliary inference never enters the foreground loop or executes returned tools.
use crate::{coding::ModelInput, runtime::InstanceIdentity};
use serde::{Deserialize, Serialize};

/// Provider-owned preparation and replay, separate from foreground wrappers and attempts.
pub const PROVIDER: &str = "eden.auxiliary-model.v1";

/// A provider-owned prepared payload. Credentials are acquired on every replay, never
/// retained in this handle. Age is measured from preparation and never renewed by replay.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct Snapshot {
    pub owner: InstanceIdentity,
    pub revision: u64,
    pub token: u64,
    pub origin_run: u64,
    pub age_ms: u64,
}

/// Latest/Prepare return `Option<Snapshot>`; unsupported safe replay returns None.
/// Replay/Generate return `coding::ModelReply` to the caller only. Callers must not
/// feed auxiliary output to the foreground queue, history, or tool dispatcher.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Request {
    Latest,
    Prepare {
        input: ModelInput,
    },
    Replay {
        snapshot: Snapshot,
        /// When true, host admission and cleanup are bounded by the originating foreground run.
        #[serde(default)]
        streaming: bool,
        max_output_tokens: u32,
        max_age_ms: u64,
        timeout_ms: u64,
    },
    Generate {
        purpose: String,
        input: ModelInput,
        timeout_ms: u64,
    },
}
