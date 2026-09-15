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
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub revision: u64,
    pub instructions: String,
    pub system: Option<String>,
    pub append_system: String,
    pub sources: Vec<String>,
    pub skills: Vec<Resource>,
    pub templates: Vec<Resource>,
    pub diagnostics: Vec<String>,
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
