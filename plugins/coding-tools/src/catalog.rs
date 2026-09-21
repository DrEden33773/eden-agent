//! The tool schemas this package publishes to the model.
use eden_plugin_sdk::{protocol::coding::ToolDefinition, serde_json::json};
pub(super) fn tools() -> Vec<ToolDefinition> {
    [
        (
            "powershell",
            "Run native PowerShell without profiles at the explicit cwd. Waits for process-tree cleanup on completion, cancellation or optional timeout_seconds deadline. Returns separate bounded stream tails and persistent full-output artifacts.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_seconds": { "type": "number", "exclusiveMinimum": 0,
                        "description": "Optional positive deadline in seconds; no default timeout." },
                },
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
            "Load an available skill on demand. Relative references resolve from its skill directory.",
            json!({
                "type": "object",
                "properties": { "name": { "type": "string" }, "arguments": { "type": "string" } },
                "required": ["name"],
                "additionalProperties": false,
            }),
        ),
        (
            "read",
            "Read UTF-8 text or PNG/JPEG/GIF/WebP images. Text uses 1-based offset and line limit; follow details.next_offset/next_byte_offset to continue.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "minimum": 1 },
                    "limit": { "type": "integer", "minimum": 1 },
                    "byte_offset": { "type": "integer", "minimum": 0 },
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
            "Apply old_text/new_text or a batch of edits against the original file. All matches must be unique and nonoverlapping. Strict mode normalizes CRLF; tolerant mode additionally normalizes curly quotes, Unicode dashes and trailing spaces/tabs. Preserves BOM and newline style.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_text": { "type": "string" },
                    "new_text": { "type": "string" },
                    "mode": { "type": "string", "enum": ["strict", "tolerant"] },
                    "edits": {
                        "type": "array", "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": { "old_text": { "type": "string" }, "new_text": { "type": "string" } },
                            "required": ["old_text", "new_text"], "additionalProperties": false,
                        },
                    },
                },
                "required": ["path"],
                "oneOf": [
                    { "required": ["old_text", "new_text"], "not": { "required": ["edits"] } },
                    { "required": ["edits"], "not": { "anyOf": [{ "required": ["old_text"] }, { "required": ["new_text"] }] } },
                ],
                "additionalProperties": false,
            }),
        ),
        (
            "bash",
            "Run bash in the session cwd. Returns separate bounded stream tails, persistent full-output artifacts and the exit code after process-tree cleanup. An optional timeout_seconds deadline stops this command; there is no default timeout.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_seconds": { "type": "number", "exclusiveMinimum": 0,
                        "description": "Optional positive deadline in seconds; no default timeout." },
                },
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
