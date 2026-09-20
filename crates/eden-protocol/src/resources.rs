//! Resource and contribution messages, independent of discovery implementations.
use crate::coding::{Block, ToolDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Resource discovery: the instruction text a session starts from and the
/// skill and template inventories it can expand.
pub const SOURCE: &str = "eden.resource-source.v1";
/// The tools a contribution makes available to the model.
pub const TOOL_CATALOG: &str = "eden.tool-catalog.v1";
/// A family of contributed commands.
pub const COMMAND: &str = "eden.command.v1";
/// A hook that may rewrite submitted content before the model sees it.
pub const BEFORE_INPUT: &str = "eden.before-input.v1";
/// A hook that may rewrite a tool call before it executes.
pub const BEFORE_TOOL: &str = "eden.before-tool.v1";
/// A search tool contributed to the coding loop.
pub const SEARCH: &str = "eden.search-tool.v1";

/// What a resource source is told: the resolved project directory, the global
/// directory, whether the project is trusted, and the settings to apply.
/// Discovery reads the filesystem itself; the host hands over no file contents.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct SourceConfig {
    pub cwd: String,
    pub global_dir: String,
    pub trusted: bool,
    #[serde(default)]
    pub settings: Value,
    #[serde(default)]
    pub skill_paths: Vec<String>,
    #[serde(default)]
    pub template_paths: Vec<String>,
}
/// One skill or template a source publishes.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct Resource {
    pub name: String,
    pub description: String,
    pub path: String,
    pub model_invocable: bool,
}
/// How much attention one diagnostic deserves.
///
/// The enum is non-exhaustive, so a reader must handle a level its own release
/// does not define. Additivity in the data path comes from the pairing string
/// rather than from that attribute: derived deserialization rejects a level it
/// does not know, and a producer can only send one this pairing defines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub enum Level {
    Info,
    Warning,
    Error,
}

/// One resource-loading diagnostic with the level its producer chose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct Diagnostic {
    pub level: Level,
    pub message: String,
}

impl Diagnostic {
    /// A diagnostic at an explicit level, for a producer that does not use the
    /// [`warning`](Diagnostic::warning) shortcut.
    pub fn new(level: Level, message: impl Into<String>) -> Self {
        Self {
            level,
            message: message.into(),
        }
    }
    /// A resource that was skipped without failing the load.
    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(Level::Warning, message)
    }
}

/// What a session loads from its resource sources: the instruction text, the
/// inventories, and every diagnostic raised while loading. `revision` advances
/// on each reload and is what a later input hook reports it acted on.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct Snapshot {
    pub revision: u64,
    pub instructions: String,
    pub system: Option<String>,
    pub append_system: String,
    pub sources: Vec<String>,
    pub skills: Vec<Resource>,
    pub templates: Vec<Resource>,
    pub diagnostics: Vec<Diagnostic>,
}
/// One operation on the loaded resources: read the current [`Snapshot`], reload
/// from disk, expand template text, or expand one skill by name.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub enum ResourceRequest {
    Snapshot,
    Reload,
    Expand { text: String },
    Skill { name: String, arguments: String },
}
/// The snapshot after an operation, plus the text an expansion produced.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct ResourceReply {
    pub snapshot: Snapshot,
    pub text: Option<String>,
}
/// A request for the tool inventory of one project directory.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct CatalogRequest {
    pub cwd: String,
}
/// The tools one catalog contributes to the model request.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct Catalog {
    pub tools: Vec<ToolDefinition>,
}
/// What a before-input hook receives: the submitted content and the resource
/// revision it is being applied to.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct InputHook {
    pub content: Vec<Block>,
    pub resource_revision: u64,
}

/// One contributed command, described well enough for a caller to spell it.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct CommandDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}
/// One invocation of a contributed command with its JSON arguments.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct CommandRequest {
    pub cwd: String,
    pub name: String,
    pub arguments: Value,
}
/// Every command the installed packages contribute.
#[derive(Clone, Debug, Serialize, Deserialize)]
// Field and variant names state the payload; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct CommandCatalog {
    pub commands: Vec<CommandDefinition>,
}
