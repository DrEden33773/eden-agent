//! Public data records shared by the host and native plugin SDK.
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Exact host/SDK pairing for this development release.
pub const CONTRACT: &str = "eden-native-0.1.0";
/// Agent loop role.
pub const AGENT_LOOP: &str = "eden.agent-loop.v1";
/// Context projection role.
pub const CONTEXT: &str = "eden.context.v1";
/// Model provider role.
pub const PROVIDER: &str = "eden.provider.v1";
/// Tool role.
pub const TOOL: &str = "eden.tool.v1";

/// Structured failure with its owning source.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Fault {
    pub code: String,
    pub source: String,
    pub message: String,
}
impl Fault {
    /// Construct a non-retryable failure; retry policy is outside this release.
    pub fn new(code: &str, source: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            source: source.into(),
            message: message.into(),
        }
    }
}
impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.code, self.source, self.message)
    }
}
impl std::error::Error for Fault {}

/// Root outcome, fixed before cleanup starts.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
pub enum Outcome {
    Completed(Value),
    Failed(Fault),
    Cancelled,
}
/// One operation's result after registered cleanup completes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Terminal {
    pub outcome: Outcome,
    pub cleanup_errors: Vec<Fault>,
}
impl Terminal {
    pub fn failed(error: Fault) -> Self {
        Self {
            outcome: Outcome::Failed(error),
            cleanup_errors: vec![],
        }
    }
    pub fn into_result(self) -> Result<Value, Fault> {
        if let Some(error) = self.cleanup_errors.into_iter().next() {
            return Err(error);
        }
        match self.outcome {
            Outcome::Completed(value) => Ok(value),
            Outcome::Failed(error) => Err(error),
            Outcome::Cancelled => Err(Fault::new("Cancelled", "operation", "cancelled")),
        }
    }
}

/// Every invocation belongs to a host-assigned session and run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub session_id: u64,
    pub run_id: u64,
    pub contract: String,
    pub payload: Value,
}
/// Ordered, inspectable session event.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub sequence: u64,
    pub session_id: u64,
    pub run_id: u64,
    pub kind: String,
    pub payload: Value,
}
/// Metadata returned by a library and matched against its installation manifest.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Descriptor {
    pub package: String,
    pub version: String,
    pub provides: Vec<String>,
}
/// One explicitly enabled local package.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackageManifest {
    pub descriptor: Descriptor,
    pub host: String,
    pub sdk: String,
    pub target: String,
    pub library: String,
    #[serde(default)]
    pub config: Value,
}
/// Resolved local composition. Paths are relative to this file, never caller cwd.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Composition {
    pub packages: Vec<PackageManifest>,
    pub roles: std::collections::BTreeMap<String, String>,
}
/// Input to the loop and context strategy.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunInput {
    pub prompt: String,
}
/// Model-visible context and prior tool results.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelInput {
    pub text: String,
    pub tool_result: Option<String>,
}
/// Controlled provider response.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "text", rename_all = "snake_case")]
pub enum ModelReply {
    ToolCall(String),
    Answer(String),
}
