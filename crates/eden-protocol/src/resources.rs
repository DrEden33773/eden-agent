//! Resource and contribution messages, independent of discovery implementations.
use crate::coding::{Block, ToolDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SOURCE: &str = "eden.resource-source.v1";
pub const TOOL_CATALOG: &str = "eden.tool-catalog.v1";
pub const COMMAND: &str = "eden.command.v1";
pub const BEFORE_INPUT: &str = "eden.before-input.v1";
pub const BEFORE_TOOL: &str = "eden.before-tool.v1";
pub const SEARCH: &str = "eden.search-tool.v1";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
pub enum Level {
    Info,
    Warning,
    Error,
}

/// One resource-loading diagnostic with the level its producer chose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub level: Level,
    pub message: String,
}

impl Diagnostic {
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum ResourceRequest {
    Snapshot,
    Reload,
    Expand { text: String },
    Skill { name: String, arguments: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceReply {
    pub snapshot: Snapshot,
    pub text: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CatalogRequest {
    pub cwd: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Catalog {
    pub tools: Vec<ToolDefinition>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputHook {
    pub content: Vec<Block>,
    pub resource_revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandRequest {
    pub cwd: String,
    pub name: String,
    pub arguments: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandCatalog {
    pub commands: Vec<CommandDefinition>,
}
