//! Read-only original-history lookup shared by context plugins and model tools.
use eden_plugin_sdk::{
    CallContext, Package,
    protocol::{
        Fault,
        coding::{
            STORE, StoreReply, StoreRequest, ToolDefinition, ToolExecution, ToolRequest, ToolResult,
        },
        history::{active_path, branch_state},
        recall::*,
        resources::{Catalog, CatalogRequest},
    },
    serde_json::{self, json},
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u32,
    session_id: u64,
    branch: String,
    head: Option<u64>,
    revision: u64,
    query: RecallQuery,
    index: usize,
    offset: usize,
}
fn fault(code: &str, message: impl Into<String>) -> Fault {
    Fault::new(code, "history-recall", message)
}
pub(super) fn register(package: Package) -> Package {
    package
        .service(RECALL, |request: RecallRequest, cx| async move {
            read(&cx, request).await
        })
        .service(TOOLS, |_: CatalogRequest, _| async move { Ok(catalog()) })
        .service(TOOL, |request: ToolRequest, cx| async move {
            if request.name != "history_recall" {
                return Err(fault("UnknownTool", "expected history_recall"));
            }
            let request = serde_json::from_value(request.arguments)
                .map_err(|e| fault("InvalidInput", e.to_string()))?;
            let reply = read(&cx, request).await?;
            let details =
                serde_json::to_value(&reply).map_err(|e| fault("RecallFailure", e.to_string()))?;
            Ok(ToolResult {
                text: details.to_string(),
                details,
                truncated: reply.truncated,
                content: vec![],
                artifacts: vec![],
                exit_code: None,
                error: None,
            })
        })
}

async fn read(cx: &CallContext, request: RecallRequest) -> Result<RecallReply, Fault> {
    let store: StoreReply = cx.call(STORE, &StoreRequest::Read).await?;
    if store.session_id != cx.session_id() {
        return Err(fault(
            "SessionMismatch",
            "store returned a different session",
        ));
    }
    recall(store, request)
}
fn catalog() -> Catalog {
    Catalog {
        tools: vec![ToolDefinition {
            name: "history_recall".into(),
            description: "Search or read original history payload JSON on the current branch. \
                          Search uses case-sensitive literal substrings, not regex. Results \
                          include source identities and UTF-8 byte ranges. Continue clipped \
                          results with next_cursor and the same query; changed history requires a \
                          new query. Never executes historical tools."
                .into(),
            execution: ToolExecution::Parallel,
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["query"],
                "properties": {
                    "query": {
                        "oneOf": [
                            {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["operation", "literal"],
                                "properties": {
                                    "operation": { "const": "search" },
                                    "literal": {
                                        "type": "string",
                                        "minLength": 1,
                                        "maxLength": 4096,
                                    },
                                    "from_sequence": { "type": ["integer", "null"], "minimum": 1 },
                                    "through_sequence": {
                                        "type": ["integer", "null"],
                                        "minimum": 1,
                                    },
                                },
                            },
                            {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["operation", "sequence"],
                                "properties": {
                                    "operation": { "const": "read" },
                                    "sequence": { "type": "integer", "minimum": 1 },
                                },
                            }
                        ],
                    },
                    "max_bytes": {
                        "type": "integer",
                        "minimum": 4,
                        "maximum": 65536,
                        "default": 8192,
                    },
                    "max_records": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 128,
                        "default": 16,
                    },
                    "cursor": { "type": ["string", "null"] },
                },
            }),
        }],
    }
}
fn validate(request: &RecallRequest) -> Result<(), Fault> {
    if !(4..=65536).contains(&request.max_bytes) || !(1..=128).contains(&request.max_records) {
        return Err(fault(
            "InvalidInput",
            "max_bytes must be 4..65536 and max_records 1..128",
        ));
    }
    match &request.query {
        RecallQuery::Search {
            literal,
            from_sequence,
            through_sequence,
        } => {
            if literal.is_empty()
                || literal.len() > 4096
                || *from_sequence == Some(0)
                || *through_sequence == Some(0)
                || matches!((from_sequence, through_sequence), (Some(from), Some(through)) if from > through)
            {
                return Err(fault(
                    "InvalidInput",
                    "search requires 1..4096 literal bytes and an ordered positive sequence range",
                ));
            }
        }
        RecallQuery::Read { sequence: 0 } => {
            return Err(fault("InvalidInput", "sequence must be positive"));
        }
        RecallQuery::Read { .. } => {}
    }
    Ok(())
}
fn recall(store: StoreReply, request: RecallRequest) -> Result<RecallReply, Fault> {
    validate(&request)?;
    let (head, branch) = branch_state(&store.records)?;
    if store.active_head != head
        || store.active_branch != branch
        || store.sequence != store.records.len() as u64
        || store
            .records
            .first()
            .is_some_and(|r| r.session_id != store.session_id)
    {
        return Err(fault(
            "InvalidHistory",
            "store snapshot identity disagrees with public history",
        ));
    }
    let mut path = active_path(&store.records)?;
    let mut cursor = if let Some(value) = &request.cursor {
        if value.len() > 32768 {
            return Err(fault("InvalidCursor", "cursor exceeds 32768 bytes"));
        }
        let cursor: Cursor =
            serde_json::from_str(value).map_err(|e| fault("InvalidCursor", e.to_string()))?;
        if cursor.version != 1 {
            return Err(fault("InvalidCursor", "unsupported cursor version"));
        }
        if cursor.session_id != store.session_id
            || cursor.branch != branch
            || cursor.revision > store.sequence
            || cursor.revision == 0
            || !path
                .iter()
                .any(|record| Some(record.sequence) == cursor.head)
            || store
                .records
                .iter()
                .skip(cursor.revision as usize)
                .any(|record| record.kind == "branch_selected")
            || cursor.query != request.query
        {
            return Err(fault(
                "StaleCursor",
                "session, branch, original path or query changed, or history was navigated; \
                 restart recall",
            ));
        }
        let (original_head, original_branch) =
            branch_state(&store.records[..cursor.revision as usize])?;
        if cursor.head != original_head || cursor.branch != original_branch {
            return Err(fault(
                "StaleCursor",
                "original snapshot identity no longer agrees with history",
            ));
        }
        // Store records are immutable. Retain the original ancestor path while
        // allowing later tool results and model turns to append to this branch.
        path.retain(|record| Some(record.sequence) <= cursor.head);
        if cursor.index >= path.len() {
            return Err(fault(
                "InvalidCursor",
                "cursor position is outside the active path",
            ));
        }
        cursor
    } else {
        Cursor {
            version: 1,
            session_id: store.session_id,
            branch: branch.clone(),
            head,
            revision: store.sequence,
            query: request.query.clone(),
            index: 0,
            offset: 0,
        }
    };
    if let RecallQuery::Read { sequence } = request.query
        && !path.iter().any(|r| r.sequence == sequence)
    {
        return Err(fault(
            "RecordNotFound",
            "record is not on the current session's active branch",
        ));
    }
    let mut reply = RecallReply {
        session_id: store.session_id,
        active_branch: branch,
        active_head: cursor.head,
        revision: cursor.revision,
        chunks: vec![],
        truncated: false,
        next_cursor: None,
    };
    let mut remaining = request.max_bytes;
    while let Some(record) = path.get(cursor.index) {
        let text = record.payload.to_string();
        let selected = match &request.query {
            RecallQuery::Read { sequence } => record.sequence == *sequence,
            RecallQuery::Search {
                literal,
                from_sequence,
                through_sequence,
            } => {
                from_sequence.is_none_or(|start| record.sequence >= start)
                    && through_sequence.is_none_or(|end| record.sequence <= end)
                    && text.contains(literal)
            }
        };
        if !selected {
            if cursor.offset != 0 {
                return Err(fault(
                    "InvalidCursor",
                    "offset does not address a matching record",
                ));
            }
            cursor.index += 1;
            continue;
        }
        if cursor.offset >= text.len() || !text.is_char_boundary(cursor.offset) {
            return Err(fault(
                "InvalidCursor",
                "offset is outside the payload or splits UTF-8",
            ));
        }
        let end = text.floor_char_boundary(cursor.offset.saturating_add(remaining).min(text.len()));
        if reply.chunks.len() >= request.max_records || end == cursor.offset {
            break;
        }
        reply.chunks.push(RecallChunk {
            session_id: record.session_id,
            sequence: record.sequence,
            branch: record.branch.clone(),
            run_id: record.run_id,
            kind: record.kind.clone(),
            byte_start: cursor.offset,
            byte_end: end,
            total_bytes: text.len(),
            text: text[cursor.offset..end].into(),
        });
        remaining -= end - cursor.offset;
        if end < text.len() {
            cursor.offset = end;
            break;
        }
        cursor.offset = 0;
        cursor.index += 1;
    }
    if cursor.index < path.len() {
        reply.truncated = true;
        reply.next_cursor = Some(
            serde_json::to_string(&cursor).map_err(|e| fault("RecallFailure", e.to_string()))?,
        );
    }
    Ok(reply)
}

#[cfg(test)]
mod tests;
