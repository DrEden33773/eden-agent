//! The tool schemas this package publishes to the model.
use eden_plugin_sdk::{protocol::coding::ToolDefinition, serde_json::json};
pub(super) fn tools() -> Vec<ToolDefinition> {
    [
        (
            "powershell",
            concat!(
                "Run native PowerShell without profiles at the explicit cwd. Waits for process-tree ",
                "cleanup on completion or cancellation.",
            ),
            json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"],
                "additionalProperties": false,
            }),
        ),
        (
            "ls",
            "List immediate directory entries in name order.",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "additionalProperties": false,
            }),
        ),
        (
            "skill",
            concat!(
                "Load an available skill on demand. Relative references resolve from its skill ",
                "directory.",
            ),
            json!({
                "type": "object",
                "properties": { "name": { "type": "string" }, "arguments": { "type": "string" } },
                "required": ["name"],
                "additionalProperties": false,
            }),
        ),
        (
            "read",
            "Read a UTF-8 file. offset is a 1-based line; limit bounds lines.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "minimum": 1 },
                    "limit": { "type": "integer", "minimum": 1 },
                },
                "required": ["path"],
                "additionalProperties": false,
            }),
        ),
        (
            "write",
            "Write a UTF-8 file, creating parent directories.",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" }, "content": { "type": "string" } },
                "required": ["path", "content"],
                "additionalProperties": false,
            }),
        ),
        (
            "edit",
            concat!(
                "Replace exactly one occurrence of old_text with new_text. Fails without changing ",
                "the file if absent or ambiguous.",
            ),
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_text": { "type": "string" },
                    "new_text": { "type": "string" },
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false,
            }),
        ),
        (
            "bash",
            "Run a bash command in the session cwd. Returns bounded output and the exit code.",
            json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"],
                "additionalProperties": false,
            }),
        ),
    ]
    .into_iter()
    .map(|(name, description, parameters)| ToolDefinition {
        name: name.into(),
        description: description.into(),
        parameters,
    })
    .collect()
}
