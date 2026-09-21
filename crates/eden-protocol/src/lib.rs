//! Public data records shared by the host and native plugin SDK.
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub mod resources;

/// Exact host/SDK pairing for this development release.
pub const CONTRACT: &str = "eden-native-0.3.0";
/// Agent loop role.
pub const AGENT_LOOP: &str = "eden.agent-loop.v1";
/// Context projection role.
pub const CONTEXT: &str = "eden.context.v1";
/// Model provider role.
pub const PROVIDER: &str = "eden.provider.v1";
/// Tool role.
pub const TOOL: &str = "eden.tool.v1";
/// Optional instance finalizer, invoked by the host after normal admission drains.
pub const INSTANCE_STOP: &str = "eden.instance-stop.v1";

/// Structured failure with its owning source.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct Fault {
    pub code: String,
    pub source: String,
    pub message: String,
}
impl Fault {
    /// Construct a structured failure; the caller owns retry policy.
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
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub enum Outcome {
    Completed(Value),
    Failed(Fault),
    Cancelled,
}
/// One operation's result after registered cleanup completes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct Terminal {
    pub outcome: Outcome,
    pub cleanup_errors: Vec<Fault>,
}
impl Terminal {
    /// A terminal that failed before any cleanup was registered, so it carries
    /// no cleanup errors.
    pub fn failed(error: Fault) -> Self {
        Self {
            outcome: Outcome::Failed(error),
            cleanup_errors: vec![],
        }
    }
    /// Reduce the terminal to a result. A cleanup error outranks the outcome,
    /// because a run whose teardown failed is not a success; a cancelled run
    /// without cleanup errors becomes the cancellation fault callers expect.
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
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct Request {
    pub session_id: u64,
    pub run_id: u64,
    pub contract: String,
    pub payload: Value,
}
/// Ordered, inspectable session event.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct Event {
    pub sequence: u64,
    pub session_id: u64,
    pub run_id: u64,
    pub kind: String,
    pub payload: Value,
}
/// Metadata returned by a library and matched against its installation manifest.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct Descriptor {
    pub package: String,
    pub version: String,
    pub provides: Vec<String>,
}
/// One explicitly enabled local package.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct PackageManifest {
    pub descriptor: Descriptor,
    pub host: String,
    pub sdk: String,
    pub target: String,
    pub library: String,
    #[serde(default)]
    pub config: Value,
    /// Required selected contracts, including domains unknown to the host.
    #[serde(default)]
    pub requires: Vec<String>,
}
/// Resolved local composition. Paths are relative to this file, never caller cwd.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct Composition {
    pub packages: Vec<PackageManifest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resource_packages: Vec<resources::LockedResourcePackage>,
    pub roles: std::collections::BTreeMap<String, String>,
}
/// Input to the loop and context strategy.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct RunInput {
    pub prompt: String,
}
/// Model-visible context and prior tool results.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct ModelInput {
    pub text: String,
    pub tool_result: Option<String>,
}
/// Controlled provider response.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "text", rename_all = "snake_case")]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub enum ModelReply {
    ToolCall(String),
    Answer(String),
}

/// Public coding and persistence protocols.
pub mod coding;

/// Public tree validation and standalone readable history.
pub mod history;
