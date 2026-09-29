//! Record projection and compact or summary requests for the default context role.
use super::*;
use eden_plugin_sdk::protocol::compaction as c;

use eden_plugin_sdk::protocol::context_edit::{Document, Entry};
use eden_plugin_sdk::protocol::history::active_path;
use std::collections::BTreeMap;

fn text_item(text: String) -> Item {
    Item::Message {
        role: "user".into(),
        content: vec![Block::Text { text }],
    }
}
fn decode(record: &Record) -> Result<Item, Fault> {
    serde_json::from_value(record.payload.clone())
        .map_err(|error| Fault::new("PersistenceFailure", "context", error.to_string()))
}
pub(crate) fn project_records(records: &[Record]) -> Result<Vec<Item>, Fault> {
    let path = active_path(records)?;
    validate_compactions(&path)?;
    project_path(&path, records)
}
fn validate_compactions(path: &[Record]) -> Result<(), Fault> {
    for record in path.iter().filter(|record| record.kind == "compaction") {
        let valid_cut = record.payload["first_kept"].as_u64().is_some_and(|first| {
            first == 0
                || (first < record.sequence && path.iter().any(|item| item.sequence == first))
        });
        let valid_summary = record.payload["summary"]
            .as_str()
            .is_some_and(|summary| !summary.trim().is_empty());
        if !valid_cut || !valid_summary {
            return Err(Fault::new(
                "PersistenceFailure",
                "context",
                format!(
                    "Invalid compaction summary or ancestor cut at record {}",
                    record.sequence
                ),
            ));
        }
    }
    Ok(())
}
fn project_path(path: &[Record], records: &[Record]) -> Result<Vec<Item>, Fault> {
    Ok(project_entries(path, records)?
        .into_iter()
        .map(|entry| entry.item)
        .collect())
}
fn project_entries(path: &[Record], records: &[Record]) -> Result<Vec<Entry>, Fault> {
    let rebuild = path
        .iter()
        .rev()
        .find(|record| record.kind == "context_rebuild")
        .map_or(0, |record| record.sequence);
    let compact = path
        .iter()
        .rev()
        .find(|record| record.kind == "compaction" && record.sequence > rebuild);
    let first = compact
        .and_then(|record| record.payload.get("first_kept"))
        .and_then(Value::as_u64);
    let states = queue::statuses(records);
    let mut items = vec![];
    let mut identities = vec![];
    let mut retained_references = BTreeMap::new();
    if let Some(record) = compact {
        items.push(text_item(format!(
            "Previous context summary:\n{}",
            record.payload["summary"].as_str().unwrap_or_default()
        )));
        add_uncertainties(&mut items, &record.payload);
        identities
            .extend((0..items.len()).map(|index| format!("record:{}:{index}", record.sequence)));
        if let Some(retained) = record.payload.get("context_retained") {
            let retained: Vec<Entry> = serde_json::from_value(retained.clone())
                .map_err(|error| Fault::new("PersistenceFailure", "context", error.to_string()))?;
            for entry in retained {
                retained_references.insert(entry.id.clone(), entry.references);
                identities.push(entry.id);
                items.push(entry.item);
            }
        }
    }
    for record in path {
        if let Some(first) = first
            && record.sequence
                < if first == 0 {
                    compact.unwrap().sequence + 1
                } else {
                    first
                }
        {
            continue;
        }
        let start = items.len();
        match record.kind.as_str() {
            "message" | "tool_intent" | "tool_result" | "provider_state" => {
                items.push(decode(record)?)
            }
            "queue_delivered" => {
                let id = record.payload["id"].as_u64().unwrap_or(0);
                if let Some((state, last)) = states.get(&id)
                    && (*state == "queue_consumed"
                        || (*state == "queue_delivered" && last.sequence == record.sequence))
                    && !path.iter().any(|later| {
                        later.kind == "queue_delivered"
                            && later.sequence > record.sequence
                            && later.payload["id"] == record.payload["id"]
                    })
                {
                    let entry: QueueEntry = serde_json::from_value(record.payload.clone())
                        .map_err(|e| Fault::new("PersistenceFailure", "queue", e.to_string()))?;
                    items.push(Item::Message {
                        role: "user".into(),
                        content: entry.content,
                    });
                }
            }
            "user_shell" if record.payload["exclude_from_context"] != true => {
                let result: ToolResult = serde_json::from_value(record.payload["result"].clone())
                    .map_err(|error| {
                    Fault::new("PersistenceFailure", "user-shell", error.to_string())
                })?;
                let execution = &result.details["execution"];
                let command = execution["command"]
                    .as_str()
                    .or_else(|| record.payload["command"].as_str())
                    .unwrap_or_default();
                let shell = execution["shell"]
                    .as_str()
                    .or_else(|| record.payload["shell"].as_str())
                    .unwrap_or_default();
                let status = result.error.as_ref().map_or_else(
                    || {
                        result.exit_code.map_or_else(
                            || "exit code unavailable".into(),
                            |code| format!("exit code {code}"),
                        )
                    },
                    |error| format!("{}: {}", error.code, error.message),
                );
                let mut content = vec![Block::Text {
                    text: format!(
                        "User shell execution ({shell}):\n{command}\nStatus: {status}\n{}",
                        result.text
                    ),
                }];
                content.extend(result.content);
                items.push(Item::Message {
                    role: "user".into(),
                    content,
                });
            }
            "branch_summary" => {
                items.push(text_item(format!(
                    "Explicit context carried from another branch:\n{}",
                    record.payload["summary"].as_str().unwrap_or_default()
                )));
                add_uncertainties(&mut items, &record.payload);
            }
            _ => {}
        }
        identities.extend(
            (0..items.len() - start).map(|index| format!("record:{}:{index}", record.sequence)),
        );
    }
    let by_sequence: BTreeMap<_, _> = records
        .iter()
        .map(|record| (record.sequence, record))
        .collect();
    let completed: BTreeSet<_> = items
        .iter()
        .filter_map(|item| {
            if let Item::ToolResult { call_id, .. } = item {
                Some(call_id.clone())
            } else {
                None
            }
        })
        .collect();
    items
        .into_iter()
        .zip(identities)
        .map(|(item, id)| {
            let references = id
                .strip_prefix("record:")
                .and_then(|id| id.split(':').next())
                .and_then(|id| id.parse::<u64>().ok())
                .and_then(|sequence| by_sequence.get(&sequence))
                .and_then(|record| record.payload.get("references"))
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|error| Fault::new("InvalidReference", "context", error.to_string()))?
                .unwrap_or_default();
            let references = retained_references.get(&id).cloned().unwrap_or(references);
            Ok(Entry {
                id,
                references,
                item: match item {
                    Item::ToolCall { ref call_id, .. } if !completed.contains(call_id) => {
                        text_item(format!(
                            "Historical tool call {call_id} was interrupted; result and external \
                             effects are unknown. It was not replayed."
                        ))
                    }
                    other => other,
                },
            })
        })
        .collect::<Result<Vec<_>, Fault>>()
}
fn add_uncertainties(items: &mut Vec<Item>, payload: &Value) {
    for (key, label) in [
        ("read_files", "Cumulative read paths"),
        ("modified_files", "Cumulative write/edit paths"),
    ] {
        if let Some(files) = payload[key].as_array()
            && !files.is_empty()
        {
            items.push(text_item(format!("{label}: {}", json!(files))));
        }
    }
    if let Some(values) = payload["uncertainties"].as_array()
        && !values.is_empty()
    {
        items.push(text_item(format!(
            "Unresolved historical tool effects (not replayed; verify before repeating): {}",
            json!(values)
        )));
    }
}
pub(crate) fn estimate(items: &[Item]) -> u64 {
    items
        .iter()
        .map(|item| match item {
            Item::Message { content, .. } => {
                content
                    .iter()
                    .map(|block| match block {
                        Block::Text { text } => text.chars().count() as u64 / 4 + 1,
                        // Binary attachment size is not text token usage. Until provider usage is
                        // available use a bounded block allowance, never base64 character count.
                        _ => 1024,
                    })
                    .sum::<u64>()
                    + 4
            }
            Item::ToolResult { result, .. } => {
                result.text.chars().count() as u64 / 4
                    + 1
                    + result
                        .content
                        .iter()
                        .map(|block| match block {
                            Block::Text { text } => text.chars().count() as u64 / 4 + 1,
                            _ => 1024,
                        })
                        .sum::<u64>()
                    + result.details.to_string().chars().count() as u64 / 4
                    + 1
                    + serde_json::to_string(&result.artifacts)
                        .map_or(0, |s| s.chars().count() as u64 / 4 + 1)
            }
            _ => serde_json::to_string(item).map_or(0, |s| s.chars().count() as u64 / 4 + 1),
        })
        .sum()
}
pub(crate) fn generation(path: &[Record]) -> u64 {
    path.iter()
        .rev()
        .find(|r| {
            matches!(
                r.kind.as_str(),
                "compaction" | "context_edit" | "context_rebuild" | "image_version"
            )
        })
        .map_or(0, |r| r.sequence)
}
pub(crate) fn usage_tokens(usage: &Value) -> Option<u64> {
    usage["total_tokens"].as_u64().or_else(|| {
        Some(
            usage["input_tokens"]
                .as_u64()?
                .saturating_add(usage["output_tokens"].as_u64()?),
        )
    })
}
fn calibrated_estimate(path: &[Record], estimate: u64) -> u64 {
    let current = generation(path);
    path.iter()
        .rev()
        // Copies renumber checkpoints. A matching number before the latest
        // compaction is still stale evidence for the current projection.
        .take_while(|record| {
            !matches!(
                record.kind.as_str(),
                "compaction" | "context_edit" | "context_rebuild" | "image_version"
            )
        })
        .find_map(|record| {
            if record.kind != "model_response"
                || record.payload["usage_generation"].as_u64()? != current
            {
                return None;
            }
            let total = usage_tokens(&record.payload["usage"])?;
            let baseline = record.payload["response_estimate"].as_u64()?;
            Some(total.saturating_add(estimate.saturating_sub(baseline)))
        })
        .unwrap_or(estimate)
}

fn retained_cut(path: &[Record], keep_recent_tokens: u64) -> Result<usize, Fault> {
    // Keep about 20k recent tokens. A cut may only precede a complete assistant/tool round.
    let rebuild = path
        .iter()
        .rev()
        .find(|record| record.kind == "context_rebuild")
        .map_or(0, |record| record.sequence);
    let first = path
        .iter()
        .rev()
        .find(|record| record.kind == "compaction" && record.sequence > rebuild)
        .map(|record| {
            record.payload["first_kept"]
                .as_u64()
                .filter(|n| *n > 0)
                .unwrap_or(record.sequence + 1)
        })
        .unwrap_or(0);
    let start = path
        .iter()
        .position(|record| record.sequence >= first)
        .unwrap_or(path.len());
    let mut tokens = 0;
    let mut cut = path.len();
    for (index, record) in path.iter().enumerate().skip(start).rev() {
        if !matches!(
            record.kind.as_str(),
            "message"
                | "tool_intent"
                | "tool_result"
                | "provider_state"
                | "queue_delivered"
                | "branch_summary"
                | "user_shell"
        ) || (record.kind == "user_shell" && record.payload["exclude_from_context"] == true)
        {
            continue;
        }
        tokens +=
            serde_json::to_string(&record.payload).map_or(0, |s| s.chars().count() as u64 / 4 + 1);
        cut = index;
        if tokens >= keep_recent_tokens {
            break;
        }
    }
    if tokens < keep_recent_tokens {
        return Ok(0);
    }
    while cut > start {
        let record = &path[cut];
        let safe = record.kind == "queue_delivered"
            || record.kind == "branch_summary"
            || (record.kind == "user_shell" && record.payload["exclude_from_context"] != true)
            || (record.kind == "message"
                && matches!(decode(record)?, Item::Message { role, .. } if role == "user"));
        let round = record.kind == "provider_state"
            || (record.kind == "message"
                && matches!(decode(record)?, Item::Message { role, .. } if role == "assistant"))
            || record.kind == "tool_intent";
        if safe {
            break;
        }
        if round {
            // The entire committed response is one group regardless of ordering
            // between reasoning state, assistant text, and parallel tool intentions.
            while cut > start {
                let previous = &path[cut - 1];
                let same_response = matches!(
                    previous.kind.as_str(),
                    "provider_state" | "tool_intent"
                ) || (previous.kind == "message"
                    && matches!(decode(previous)?, Item::Message { role, .. } if role == "assistant"));
                if !same_response {
                    break;
                }
                cut -= 1;
            }
            break;
        }
        cut -= 1;
    }
    Ok(cut)
}
fn compaction_cut(path: &[Record], action: &str, settings: &Settings) -> Result<usize, Fault> {
    if action == "branch_summary" {
        Ok(path.len())
    } else if path
        .last()
        .is_some_and(|record| record.kind == "compaction")
    {
        Ok(0)
    } else {
        retained_cut(path, settings.keep_recent_tokens)
    }
}
fn evidence(path: &[Record]) -> (Vec<String>, Vec<String>, Vec<Value>) {
    let mut read = BTreeSet::new();
    let mut modified = BTreeSet::new();
    let mut calls = BTreeMap::new();
    let mut completed = BTreeSet::new();
    for record in path {
        for (key, target) in [("read_files", &mut read), ("modified_files", &mut modified)] {
            if let Some(files) = record.payload[key].as_array() {
                for file in files {
                    if let Some(file) = file.as_str() {
                        target.insert(file.to_owned());
                    }
                }
            }
        }
        if let Some(values) = record.payload["uncertainties"].as_array() {
            for value in values {
                if let Some(id) = value["call_id"].as_str() {
                    calls.insert(id.to_owned(), value.clone());
                }
            }
        }
        if record.kind == "tool_intent"
            && let Ok(Item::ToolCall {
                call_id,
                name,
                arguments,
            }) = decode(record)
        {
            if let Ok(args) = serde_json::from_str::<Value>(&arguments)
                && let Some(path) = args["path"].as_str()
            {
                if name == "read" {
                    read.insert(path.into());
                } else if name == "write" || name == "edit" {
                    modified.insert(path.into());
                }
            }
            calls.insert(
                call_id.clone(),
                json!({ "call_id": call_id, "name": name, "arguments": arguments }),
            );
        }
        if record.kind == "tool_result"
            && let Some(id) = record.payload["call_id"].as_str()
        {
            completed.insert(id.to_owned());
        }
    }
    (
        read.into_iter().collect(),
        modified.into_iter().collect(),
        calls
            .into_iter()
            .filter(|(id, _)| !completed.contains(id))
            .map(|(_, value)| value)
            .collect(),
    )
}
fn summary_prefix(path: &[Record], cut: usize, records: &[Record]) -> Result<Vec<Item>, Fault> {
    let mut prefix = path[..cut].to_vec();
    if let Some(previous) = path.iter().rev().find(|r| r.kind == "compaction")
        && !prefix.iter().any(|r| r.sequence == previous.sequence)
    {
        prefix.push(previous.clone());
    }
    project_path(&prefix, records)
}
fn summary_safe_items(items: Vec<Item>) -> Vec<Item> {
    items
        .into_iter()
        .map(|item| match item {
            Item::Message { role, content } => Item::Message {
                role,
                content: content
                    .into_iter()
                    .map(|block| match block {
                        Block::Image { media_type, .. } => Block::Text {
                            text: format!(
                                "[Image attachment {media_type}; preserved in original record]"
                            ),
                        },
                        Block::File {
                            name, media_type, ..
                        } => Block::Text {
                            text: format!(
                                "[File attachment {name} ({media_type}); preserved in original \
                                 record]"
                            ),
                        },
                        other => other,
                    })
                    .collect(),
            },
            Item::ToolResult {
                call_id,
                mut result,
            } => {
                result.content = result
                    .content
                    .into_iter()
                    .map(|block| match block {
                        Block::Text { .. } => block,
                        Block::Image { media_type, .. } => Block::Text {
                            text: format!(
                                "[Image tool output {media_type}; preserved in original record]"
                            ),
                        },
                        Block::File {
                            name, media_type, ..
                        } => Block::Text {
                            text: format!(
                                "[File tool output {name} ({media_type}); preserved in original \
                                 record]"
                            ),
                        },
                    })
                    .collect();
                Item::ToolResult { call_id, result }
            }
            Item::ProviderState { provider, .. } => text_item(format!(
                "[Opaque {provider} provider state preserved in original record]"
            )),
            other => other,
        })
        .collect()
}
fn latest_states(records: &[Record]) -> Result<Vec<ExtensionState>, Fault> {
    let rebuild = records
        .iter()
        .rev()
        .find(|record| record.kind == "context_rebuild")
        .map_or(0, |record| record.sequence);
    let mut replaced = BTreeSet::new();
    for (index, checkpoint) in records
        .iter()
        .enumerate()
        .filter(|(_, record)| record.kind == "compaction" && record.sequence < rebuild)
    {
        let mut start = index;
        while start > 0 && records[start - 1].kind == "extension_state" {
            start -= 1;
        }
        if start > 0
            && records[start - 1].kind == "model_request"
            && records[start - 1].payload["purpose"] == "compaction"
            && records[start - 1].payload["request_id"] == checkpoint.payload["request_id"]
        {
            replaced.extend(records[start..index].iter().map(|record| record.sequence));
        }
    }
    let states: Vec<ExtensionState> = records
        .iter()
        .filter(|r| r.kind == "extension_state" && !replaced.contains(&r.sequence))
        .map(|r| {
            serde_json::from_value(r.payload.clone())
                .map_err(|e| Fault::new("PersistenceFailure", "context", e.to_string()))
        })
        .collect::<Result<_, _>>()?;
    let mut latest = BTreeMap::new();
    for state in states {
        latest.insert(state.namespace.clone(), state);
    }
    Ok(latest.into_values().collect())
}
async fn interpreted(records: &[Record], cx: &CallContext) -> Result<Vec<Item>, Fault> {
    let states = latest_states(records)?;
    if states.iter().any(|state| state.required) {
        let reply: InterpretReply = cx
            .call(INTERPRETER, &InterpretRequest { states })
            .await
            .map_err(|e| {
                Fault::new(
                    "MissingInterpreter",
                    "context",
                    format!("Required extension state cannot be interpreted: {e}"),
                )
            })?;
        Ok(reply.items)
    } else {
        Ok(vec![])
    }
}
pub(crate) fn system_item(input: &ContextInput) -> Item {
    let selected = input.tools.clone().unwrap_or_else(tools);
    let mut system = input
        .resources
        .as_ref()
        .and_then(|r| r.system.clone())
        .unwrap_or_else(|| {
            "You are eden, a coding assistant. Use the available tools to inspect, change and \
             verify the project. Report observed results accurately. Tool failures are evidence to \
             address; do not claim unexecuted checks passed."
                .into()
        });
    system.push_str(&format!("\n\nWorking directory: {}", input.cwd));
    if let Some(resources) = &input.resources {
        system.push_str("\n\n");
        system.push_str(&resources.append_system);
        system.push_str(&resources.instructions);
        let skill_tool = selected.iter().any(|tool| tool.name == "skill");
        let read_tool = selected.iter().any(|tool| tool.name == "read");
        if skill_tool || read_tool {
            for skill in resources
                .skills
                .iter()
                .filter(|skill| skill.model_invocable)
            {
                let loading = if skill_tool {
                    "Use the skill tool to load its instructions on demand.".to_owned()
                } else {
                    format!(
                        "Use read with path eden-resource://skill/{} to load its frozen \
                         instructions.",
                        skill.name
                    )
                };
                system.push_str(&format!(
                    "\nAvailable skill {}: {}. {}",
                    skill.name, skill.description, loading
                ));
            }
        }
    }
    Item::Message {
        role: "system".into(),
        content: vec![Block::Text { text: system }],
    }
}
fn split_edited(document: &Document, keep: u64) -> (Vec<Item>, Vec<Entry>) {
    let entries: Vec<_> = document.entries.iter().filter(|entry| !matches!(&entry.item, Item::Message {role, ..} if role == "system" || role == "developer") && !entry.id.starts_with("extension:")).cloned().collect();
    let mut cut = entries.len();
    let mut tokens = 0;
    while cut > 0 && tokens < keep {
        cut -= 1;
        tokens += estimate(std::slice::from_ref(&entries[cut].item));
    }
    // Never start retained input in the middle of an assistant/tool response.
    while cut > 0 && !matches!(&entries[cut].item, Item::Message {role, ..} if role == "user") {
        cut -= 1;
    }
    (
        entries[..cut]
            .iter()
            .map(|entry| entry.item.clone())
            .collect(),
        entries[cut..].to_vec(),
    )
}

pub(crate) async fn document(input: &ContextInput, cx: &CallContext) -> Result<Document, Fault> {
    let path = if input.action == "branch_summary" {
        input.records.clone()
    } else {
        active_path(&input.records)?
    };
    validate_compactions(&path)?;
    let mut entries = vec![Entry {
        id: "system".into(),
        item: system_item(input),
        references: vec![],
    }];
    entries.extend(project_entries(&path, &input.records)?);
    let state_revision = path
        .iter()
        .rev()
        .find(|record| record.kind == "extension_state")
        .map_or(0, |record| record.sequence);
    entries.extend(
        interpreted(&path, cx)
            .await?
            .into_iter()
            .enumerate()
            .map(|(index, item)| Entry {
                id: format!("extension:{state_revision}:{index}"),
                item,
                references: vec![],
            }),
    );
    let mut document = Document {
        entries,
        tools: input.tools.clone().unwrap_or_else(tools),
    };
    let rebuild = path
        .iter()
        .rev()
        .find(|record| record.kind == "context_rebuild")
        .map_or(0, |record| record.sequence);
    if let Some(checkpoint) = path
        .iter()
        .rev()
        .find(|record| record.kind == "compaction" && record.sequence > rebuild)
    {
        if let Some(system) = checkpoint
            .payload
            .get("context_system")
            .filter(|value| !value.is_null())
        {
            let mut system: Vec<Entry> = serde_json::from_value(system.clone())
                .map_err(|error| Fault::new("PersistenceFailure", "context", error.to_string()))?;
            system.extend(document.entries.into_iter().filter(|entry| !matches!(&entry.item, Item::Message {role, ..} if role == "system" || role == "developer")));
            document.entries = system;
        }
        if let Some(tools) = checkpoint
            .payload
            .get("context_tools")
            .filter(|value| !value.is_null())
        {
            document.tools = serde_json::from_value(tools.clone())
                .map_err(|error| Fault::new("PersistenceFailure", "context", error.to_string()))?;
        }
    }
    Ok(document)
}

pub(crate) async fn context(
    mut input: ContextInput,
    cx: CallContext,
    settings: Settings,
) -> Result<ModelInput, Fault> {
    let settings = settings.for_target(&input.target)?;
    if ![
        "",
        "project",
        "inspect",
        "compact",
        "overflow",
        "post_response",
        "branch_summary",
    ]
    .contains(&input.action.as_str())
    {
        return Err(Fault::new(
            "InvalidInput",
            "context",
            "unknown context action",
        ));
    }
    if input.records.is_empty() && input.action != "branch_summary" {
        let history: StoreReply = cx.call(STORE, &StoreRequest::Read).await?;
        input.records = history.records;
    }
    if let Some(target) = &input.target {
        input.limits = target.limits.clone();
    } else if input.limits.context_window == 0 {
        input.limits = super::model_limits(&cx).await?;
    }
    let path = if input.action == "branch_summary" {
        input.records.clone()
    } else {
        active_path(&input.records)?
    };
    if input.action != "branch_summary" {
        validate_compactions(&path)?;
    }
    let mut extension_items = interpreted(&path, &cx).await?;
    let mut projected = project_path(&path, &input.records)?;
    projected.extend(input.items.clone());
    let threshold = matches!(input.action.as_str(), "" | "project")
        && settings.auto_compaction()
        && !super::edits::pending_temporary(&input.records)?
        && !path.iter().any(|record| {
            record.payload["references"]
                .as_array()
                .is_some_and(|references| !references.is_empty())
        })
        && input.limits.context_window > 0
        && calibrated_estimate(
            &path,
            estimate(&projected)
                + estimate(&extension_items)
                + estimate(&[system_item(&input)])
                + serde_json::to_string(&input.tools.clone().unwrap_or_else(tools))
                    .map_or(0, |s| s.chars().count() as u64 / 4 + 1),
        ) > input
            .limits
            .context_window
            .saturating_sub(settings.reserve_tokens);
    if matches!(
        input.action.as_str(),
        "compact" | "overflow" | "post_response"
    ) || input.action == "branch_summary"
        || ((input.action.is_empty() || input.action == "project") && threshold)
    {
        let before: StoreReply = cx.call(STORE, &StoreRequest::Read).await?;
        if input.action != "branch_summary"
            && serde_json::to_value(&before.records).ok()
                != serde_json::to_value(&input.records).ok()
        {
            return Err(Fault::new(
                "CheckpointConflict",
                "context",
                "projection input is stale",
            ));
        }
        let reason = match input.action.as_str() {
            "compact" => c::Reason::Manual,
            "overflow" => c::Reason::Overflow,
            "post_response" => c::Reason::PostResponse,
            "branch_summary" => c::Reason::BranchSummary,
            _ => c::Reason::Threshold,
        };
        let statuses = queue::statuses(&input.records);
        let max_cut = if reason == c::Reason::BranchSummary {
            path.len()
        } else {
            path.iter()
                .position(|record| {
                    record.kind == "queue_delivered"
                        && statuses
                            .get(&record.payload["id"].as_u64().unwrap_or(0))
                            .is_some_and(|(state, last)| {
                                *state == "queue_delivered" && last.sequence == record.sequence
                            })
                })
                .unwrap_or(path.len())
        };
        let request_id = format!("{}:{}:compaction", cx.run_id(), before.sequence + 1);
        let original_document = document(&input, &cx).await?;
        let (effective_document, edit_ids) = super::edits::apply_path(&original_document, &path)?;
        let edited = !edit_ids.is_empty()
            || effective_document
                .entries
                .iter()
                .any(|entry| !entry.references.is_empty())
            || path
                .iter()
                .any(|record| record.payload.get("context_retained").is_some());
        let (effective_prefix, retained) = if edited {
            if max_cut != path.len() {
                return Err(Fault::new(
                    "ContextConflict",
                    "context-edit",
                    "queued delivery must settle before compacting edited context",
                ));
            }
            let transient = path.iter().any(|record| {
                edit_ids.contains(&record.sequence) && record.payload["scope"] == "next_request"
            });
            if transient {
                return Err(Fault::new(
                    "ContextConflict",
                    "context-edit",
                    "temporary edits cannot be persisted into a compaction checkpoint",
                ));
            }
            let expanded = super::references::materialize(&effective_document, u64::MAX)?;
            let (prefix, retained) = split_edited(&expanded, settings.keep_recent_tokens);
            let retained = retained
                .into_iter()
                .map(|entry| {
                    effective_document
                        .entries
                        .iter()
                        .find(|source| source.id == entry.id)
                        .cloned()
                        .unwrap_or(entry)
                })
                .collect();
            (Some(summary_safe_items(prefix)), retained)
        } else {
            (None, vec![])
        };
        let request = c::Request {
            request_id: request_id.clone(),
            effective_prefix: effective_prefix.clone(),
            projected: effective_prefix
                .clone()
                .unwrap_or_else(|| summary_safe_items(projected.clone())),
            input: input.clone(),
            reason,
            suggested_cut: if edited {
                path.len()
            } else {
                compaction_cut(&path, &input.action, &settings)?
            },
            max_cut,
        };
        let plan: Option<c::Plan> = if effective_prefix.as_ref().is_some_and(Vec::is_empty) {
            None
        } else {
            cx.call(c::POLICY, &request).await?
        };
        if let Some(plan) = plan {
            if plan.cut == 0
                || plan.cut > max_cut
                || plan.summary.trim().is_empty()
                || (edited && plan.cut != path.len())
            {
                return Err(Fault::new(
                    "InvalidCheckpoint",
                    "context",
                    "policy returned an invalid cut or empty summary",
                ));
            }
            let sources: BTreeSet<_> = path.iter().map(|r| r.sequence).collect();
            let mut namespaces = BTreeSet::new();
            for state in &plan.states {
                if state.namespace.trim().is_empty()
                    || !namespaces.insert(state.namespace.clone())
                    || state.references.iter().any(|id| !sources.contains(id))
                {
                    return Err(Fault::new(
                        "InvalidCheckpoint",
                        "context",
                        "invalid extension namespace or source reference",
                    ));
                }
            }
            if plan.states.iter().any(|state| state.required) {
                let _: InterpretReply = cx
                    .call(
                        INTERPRETER,
                        &InterpretRequest {
                            states: plan.states.clone(),
                        },
                    )
                    .await?;
            }
            let (read, modified, uncertainties) = evidence(&path);
            let kind = if reason == c::Reason::BranchSummary {
                "branch_summary"
            } else {
                "compaction"
            };
            let mut payload = json!({
                "summary": plan.summary,
                "request_id": request_id,
                "usage": plan.usage,
                "reason": reason,
                "read_files": read,
                "modified_files": modified,
                "uncertainties": uncertainties,
            });
            if kind == "compaction" {
                if edited {
                    payload["context_retained"] = json!(retained);
                    payload["context_edits"] = json!(edit_ids);
                    let mut protected: BTreeSet<String> = effective_document
                        .entries
                        .iter()
                        .filter(|entry| {
                            original_document
                                .entries
                                .iter()
                                .find(|original| original.id == entry.id)
                                .is_none_or(|original| {
                                    original.item != entry.item
                                        || original.references != entry.references
                                })
                        })
                        .map(|entry| entry.id.clone())
                        .collect();
                    if let Some(previous) =
                        path.iter().rev().find(|record| record.kind == "compaction")
                    {
                        protected.extend(
                            previous.payload["context_protected"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .map(str::to_owned),
                        );
                    }
                    payload["context_protected"] = json!(protected);
                    let system: Vec<_> = effective_document.entries.iter().filter(|entry| matches!(&entry.item, Item::Message {role, ..} if role == "system" || role == "developer")).cloned().collect();
                    let unchanged = system.len() == 1
                        && system[0].id == "system"
                        && system[0].item == system_item(&input);
                    payload["context_system"] = if unchanged {
                        Value::Null
                    } else {
                        json!(system)
                    };
                    payload["context_tools"] = if serde_json::to_value(&effective_document.tools)
                        .ok()
                        != serde_json::to_value(input.tools.clone().unwrap_or_else(tools)).ok()
                    {
                        json!(effective_document.tools)
                    } else {
                        Value::Null
                    };
                }
                payload["first_kept"] = json!(path.get(plan.cut).map_or(0, |r| r.sequence));
                payload["source_ids"] = json!(
                    path[..plan.cut]
                        .iter()
                        .map(|r| r.sequence)
                        .collect::<Vec<_>>()
                );
            } else {
                payload["origin_session"] =
                    json!(path.first().map_or(cx.session_id(), |r| r.session_id));
                payload["origin_ids"] = json!(path.iter().map(|r| r.sequence).collect::<Vec<_>>());
            }
            let mut entries: Vec<_> = plan
                .states
                .into_iter()
                .map(|state| RecordDraft {
                    kind: "extension_state".into(),
                    payload: json!(state),
                })
                .collect();
            entries.insert(
                0,
                RecordDraft {
                    kind: "model_request".into(),
                    payload: json!({
                        "request_id": request_id,
                        "purpose": "compaction",
                        "target": input.target,
                    }),
                },
            );
            entries.push(RecordDraft {
                kind: kind.into(),
                payload,
            });
            if cx.scope.cancellation().is_cancelled() {
                return Err(Fault::new(
                    "Cancelled",
                    "context",
                    "checkpoint cancelled before commit",
                ));
            }
            let committed: StoreReply = cx
                .call(
                    STORE,
                    &StoreRequest::AppendChecked {
                        new_branch: None,
                        run_id: cx.run_id(),
                        session_id: before.session_id,
                        sequence: before.sequence,
                        head: before.active_head,
                        branch: before.active_branch,
                        entries,
                    },
                )
                .await?;
            cx.invalidate_snapshot().await?;
            cx.emit(
                "committed",
                json!({ "sequence": committed.sequence, "kind": kind }),
            )?;
            projected = project_records(&committed.records)?;
            input.records = committed.records.clone();
            extension_items = interpreted(&active_path(&committed.records)?, &cx).await?;
        } else {
            cx.emit(
                "compaction_skipped",
                json!({ "reason": "no_older_context" }),
            )?;
        }
    }
    let mut items = vec![system_item(&input)];
    let current_path = if input.action == "branch_summary" {
        input.records.clone()
    } else {
        active_path(&input.records)?
    };
    let rebuild = current_path
        .iter()
        .rev()
        .find(|record| record.kind == "context_rebuild")
        .map_or(0, |record| record.sequence);
    let checkpoint = current_path
        .into_iter()
        .rev()
        .find(|record| record.kind == "compaction" && record.sequence > rebuild);
    if let Some(checkpoint) = &checkpoint {
        if let Some(system) = checkpoint
            .payload
            .get("context_system")
            .filter(|value| !value.is_null())
        {
            let entries: Vec<Entry> = serde_json::from_value(system.clone())
                .map_err(|error| Fault::new("PersistenceFailure", "context", error.to_string()))?;
            items = entries.into_iter().map(|entry| entry.item).collect();
        }
        if let Some(tools) = checkpoint
            .payload
            .get("context_tools")
            .filter(|value| !value.is_null())
        {
            input.tools =
                Some(serde_json::from_value(tools.clone()).map_err(|error| {
                    Fault::new("PersistenceFailure", "context", error.to_string())
                })?);
        }
    }
    items.extend(projected);
    items.extend(extension_items);
    Ok(ModelInput {
        target: input.target.clone(),
        max_output_tokens: None,
        items,
        tools: input.tools.unwrap_or_else(tools),
    })
}

pub(crate) async fn recover_edited(
    input: &ContextInput,
    model: &ModelInput,
    cx: &CallContext,
    settings: &Settings,
) -> Result<ModelInput, Fault> {
    let document = Document { entries: model.items.iter().enumerate().map(|(index, item)| Entry {
        references: vec![], id: if matches!(item, Item::Message {role, ..} if role == "system" || role == "developer") { "system".into() } else { format!("request:{index}") }, item: item.clone(),
    }).collect(), tools: model.tools.clone() };
    let (prefix, retained) = split_edited(&document, settings.keep_recent_tokens);
    if prefix.is_empty() {
        return Err(Fault::new(
            "ContextOverflow",
            "context-edit",
            "edited request has no older complete turns to summarize; adjust context or model",
        ));
    }
    let path = active_path(&input.records)?;
    let plan: Option<c::Plan> = cx
        .call(
            c::POLICY,
            &c::Request {
                request_id: format!("{}:edited-recovery", cx.run_id()),
                input: input.clone(),
                projected: summary_safe_items(prefix.clone()),
                effective_prefix: Some(summary_safe_items(prefix)),
                reason: c::Reason::Overflow,
                suggested_cut: path.len(),
                max_cut: path.len(),
            },
        )
        .await?;
    let plan = plan.ok_or_else(|| {
        Fault::new(
            "ContextOverflow",
            "context-edit",
            "policy could not reduce the edited request",
        )
    })?;
    if plan.summary.trim().is_empty() {
        return Err(Fault::new(
            "InvalidCheckpoint",
            "context-edit",
            "policy returned an empty summary",
        ));
    }
    let mut recovered = model.clone();
    recovered.items = model.items.iter().filter(|item| matches!(item, Item::Message {role, ..} if role == "system" || role == "developer")).cloned().collect();
    recovered.items.push(text_item(format!(
        "Previous context summary:\n{}",
        plan.summary
    )));
    recovered
        .items
        .extend(retained.into_iter().map(|entry| entry.item));
    eden_plugin_sdk::protocol::context_edit::validate(&recovered)?;
    Ok(recovered)
}

pub(crate) async fn summary_policy(
    request: c::Request,
    cx: CallContext,
    settings: Settings,
) -> Result<Option<c::Plan>, Fault> {
    let settings = settings.for_target(&request.input.target)?;
    let request_id = request.request_id;
    let input = request.input;
    let path = if request.reason == c::Reason::BranchSummary {
        input.records.clone()
    } else {
        active_path(&input.records)?
    };
    let cut = request.suggested_cut.min(request.max_cut);
    if cut == 0 {
        return Ok(None);
    }
    let extension_items = interpreted(&path, &cx).await?;
    let split = cut < path.len()
        && path[cut].kind != "user_shell"
        && !(path[cut].kind == "message"
            && matches!(decode(&path[cut]), Ok(Item::Message { role, .. }) if role == "user"))
        && path[..cut].iter().any(|record| {
            (record.kind == "user_shell" && record.payload["exclude_from_context"] != true)
                || (record.kind == "message"
                    && matches!(decode(record), Ok(Item::Message { role, .. }) if role == "user"))
        });
    let allowance = settings.summary_allowance(split, input.limits.max_output_tokens);
    let mut summary_items = vec![Item::Message {
        role: "system".into(),
        content: vec![Block::Text {
            text: format!(
                "Summarize this coding conversation for continuation. Use headings: Goal, \
                 Constraints, Progress (Done/In Progress/Blocked), Key Decisions, Next Steps, \
                 Critical Context. Preserve user requirements, exact paths/functions/errors, \
                 failed checks, pending work, and unknown external tool effects. Update previous \
                 summaries rather than discarding them. Do not continue the task. {}",
                input.instructions
            ),
        }],
    }];
    let summary_projection = match request.effective_prefix {
        Some(items) => items,
        None => summary_safe_items(summary_prefix(&path, cut, &input.records)?),
    };
    summary_items.push(text_item(format!(
        "Conversation to summarize (attachments remain in raw history):\n{}",
        serde_json::to_string(&summary_projection).map_err(|e| Fault::new(
            "InvalidInput",
            "context",
            e.to_string()
        ))?
    )));
    summary_items.extend(extension_items.clone());
    cx.emit(
        "model_request",
        json!({ "request_id": request_id, "purpose": "summary", "target": input.target }),
    )?;
    let reply = super::provider_retry_with_history(
        &cx,
        &ModelInput {
            target: input.target.clone(),
            max_output_tokens: Some(allowance),
            items: summary_items,
            tools: vec![],
        },
        &settings,
        false,
    )
    .await?;
    cx.emit(
        "compaction_usage",
        json!({ "request_id": request_id, "purpose": "summary", "usage": reply.usage }),
    )?;
    if reply
        .items
        .iter()
        .any(|item| matches!(item, Item::ToolCall { .. }))
    {
        return Err(Fault::new(
            "ProviderFailure",
            "context",
            "summary attempted a tool call",
        ));
    }
    let summary: String = reply
        .items
        .iter()
        .filter_map(|item| {
            if let Item::Message { role, content } = item {
                if role == "assistant" {
                    Some(content)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .flatten()
        .filter_map(|block| {
            if let Block::Text { text } = block {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect();
    if summary.trim().is_empty() {
        return Err(Fault::new("ProviderFailure", "context", "empty summary"));
    }

    Ok(Some(c::Plan {
        cut,
        summary,
        states: vec![],
        usage: reply.usage,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(sequence: u64, kind: &str, payload: Value) -> Record {
        Record {
            sequence,
            parent_id: sequence.checked_sub(1).filter(|n| *n > 0),
            branch: "main".into(),
            schema_version: 2,
            session_id: 1,
            run_id: 1,
            kind: kind.into(),
            payload,
        }
    }
    #[test]
    fn user_shell_projects_effective_command_and_honors_context_exclusion() {
        let result = ToolResult {
            text: "shell output".into(),
            content: vec![],
            artifacts: vec![],
            exit_code: Some(0),
            truncated: false,
            error: None,
            details: json!({ "execution": { "shell": "bash", "command": "effective" } }),
        };
        let mut record = record(
            1,
            "user_shell",
            json!({
                "shell": "bash",
                "command": "original",
                "exclude_from_context": false,
                "result": result,
            }),
        );
        let projected =
            serde_json::to_string(&project_records(&[record.clone()]).unwrap()).unwrap();
        assert!(projected.contains("effective"));
        assert!(projected.contains("shell output"));
        assert!(!projected.contains("original"));
        let old = super::tests::record(1, "message", json!(text_item("older".into())));
        let mut recent = record.clone();
        recent.sequence = 2;
        recent.parent_id = Some(1);
        assert_eq!(retained_cut(&[old, recent], 1).unwrap(), 1);
        record.payload["exclude_from_context"] = json!(true);
        assert!(project_records(&[record]).unwrap().is_empty());
    }
    #[test]
    fn skill_catalog_uses_an_available_loader_and_custom_system_keeps_cwd() {
        let snapshot = r::Snapshot {
            system: Some("Custom system".into()),
            skills: vec![r::Resource {
                name: "review".into(),
                description: "Review changes".into(),
                path: "/mutable/SKILL.md".into(),
                model_invocable: true,
            }],
            ..r::Snapshot::default()
        };
        let mut input: ContextInput = serde_json::from_value(json!({
            "cwd": "/project",
            "items": [],
            "resources": snapshot,
            "tools": [],
        }))
        .unwrap();
        let render = |input: &ContextInput| serde_json::to_string(&system_item(input)).unwrap();
        let absent = render(&input);
        assert!(absent.contains("Custom system"));
        assert!(absent.contains("Working directory: /project"));
        assert!(!absent.contains("Available skill"));
        input.tools = Some(vec![ToolDefinition {
            execution: Default::default(),
            name: "read".into(),
            description: String::new(),
            parameters: Value::Null,
        }]);
        let read = render(&input);
        assert!(read.contains("eden-resource://skill/review"));
        assert!(!read.contains("Use the skill tool"));
        input.tools.as_mut().unwrap()[0].name = "skill".into();
        assert!(render(&input).contains("Use the skill tool"));
    }
    #[test]
    fn usage_is_calibrated_only_within_the_same_projection_generation() {
        let mut path = vec![record(
            1,
            "model_response",
            json!({
                "usage": { "total_tokens": 90000 },
                "usage_generation": 0,
                "request_estimate": 20,
                "response_estimate": 25,
            }),
        )];
        assert_eq!(calibrated_estimate(&path, 40), 90015);
        path.push(record(
            2,
            "compaction",
            json!({ "summary": "new", "first_kept": 0 }),
        ));
        assert_eq!(calibrated_estimate(&path, 40), 40);
        path.push(record(
            3,
            "model_response",
            json!({
                "usage": { "total_tokens": 800 },
                "usage_generation": 2,
                "response_estimate": 25,
            }),
        ));
        assert_eq!(calibrated_estimate(&path, 40), 815);
    }
    #[test]
    fn copied_usage_cannot_cross_a_newer_compaction_with_a_colliding_sequence() {
        let path = vec![
            record(
                2,
                "compaction",
                json!({ "summary": "old", "first_kept": 0 }),
            ),
            record(
                3,
                "model_response",
                json!({
                    "usage": { "total_tokens": 90000 },
                    "usage_generation": 8,
                    "response_estimate": 25,
                }),
            ),
            record(
                8,
                "compaction",
                json!({ "summary": "new", "first_kept": 0 }),
            ),
        ];
        assert_eq!(calibrated_estimate(&path, 40), 40);
    }
    #[test]
    fn failed_attempt_text_and_partial_tools_never_enter_projection() {
        let path = vec![record(
            1,
            "model_attempt",
            json!({ "status": "failed", "text": "partial secret", "tool_arguments": "{broken" }),
        )];
        assert!(project_records(&path).unwrap().is_empty());
    }
    #[test]
    fn manual_compaction_of_short_transcript_keeps_every_original_message() {
        let path = vec![
            record(
                1,
                "message",
                json!(text_item("keep user instruction".into())),
            ),
            record(
                2,
                "message",
                json!(Item::Message {
                    role: "assistant".into(),
                    content: vec![Block::Text {
                        text: "keep assistant answer".into()
                    }]
                }),
            ),
        ];
        let original = project_records(&path).unwrap();
        // The caller commits a compaction only for a positive cut, so the
        // falsifiable claim here is that this transcript has no compactable
        // prefix at all. A cut of zero is what keeps every recent message.
        let cut = compaction_cut(&path, "compact", &Settings::default()).unwrap();
        assert_eq!(
            cut, 0,
            "a short transcript must have no compactable prefix, not one that replaces recent \
             messages"
        );
        let projected = project_records(&path).unwrap();
        for item in original {
            assert!(
                projected.contains(&item),
                "manual compaction lost recent original item"
            );
        }
    }
    #[test]
    fn summarized_attachment_is_not_projected_back_to_the_provider() {
        let path = vec![
            record(
                1,
                "message",
                json!(Item::Message {
                    role: "user".into(),
                    content: vec![
                        Block::Image {
                            media_type: "image/png".into(),
                            data: "summarizedattachmentbytes".into(),
                        },
                        Block::Text {
                            text: "old attached goal".into(),
                        },
                    ],
                }),
            ),
            record(2, "message", json!(text_item("recent work".into()))),
            record(
                3,
                "compaction",
                json!({ "summary": "summary", "first_kept": 2 }),
            ),
        ];
        let projected = project_records(&path).unwrap();
        let text = serde_json::to_string(&projected).unwrap();
        assert!(
            !text.contains("summarizedattachmentbytes"),
            "an attachment replaced by the summary must not be sent again"
        );
        assert!(text.contains("recent work"), "{text}");
    }
    #[test]
    fn invalid_compaction_cut_cannot_silently_hide_conversation() {
        let path = vec![
            record(1, "message", json!(text_item("user requirement".into()))),
            record(
                2,
                "compaction",
                json!({ "summary": "summary", "first_kept": 999 }),
            ),
        ];
        assert!(project_records(&path).is_err());
    }
    #[test]
    fn obsolete_required_extension_version_does_not_block_latest_display_only_state() {
        let path = vec![
            record(
                1,
                "extension_state",
                json!({
                    "namespace": "plugin.x",
                    "version": 1,
                    "required": true,
                    "summary": "old",
                    "value": {},
                }),
            ),
            record(
                2,
                "extension_state",
                json!({
                    "namespace": "plugin.x",
                    "version": 2,
                    "required": false,
                    "summary": "new",
                    "value": {},
                }),
            ),
        ];
        let states = latest_states(&path).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].version, 2);
        assert!(!states[0].required);
    }
    #[test]
    fn second_summary_iterates_previous_summary_not_original_old_messages() {
        let path = vec![
            record(
                1,
                "message",
                json!(text_item("original forgotten details".into())),
            ),
            record(2, "message", json!(text_item("retained newer work".into()))),
            record(
                3,
                "compaction",
                json!({ "summary": "previous summary", "first_kept": 2 }),
            ),
            record(4, "message", json!(text_item("latest work".into()))),
        ];
        let items = summary_prefix(&path, 2, &path).unwrap();
        let text = serde_json::to_string(&items).unwrap();
        assert!(text.contains("previous summary"));
        assert!(text.contains("retained newer work"));
        assert!(!text.contains("original forgotten details"));
        assert!(!text.contains("latest work"));
    }
    #[test]
    fn recent_cut_does_not_split_parallel_tool_intentions_and_results() {
        let path = vec![
            record(1, "message", json!(text_item("task".into()))),
            record(
                2,
                "provider_state",
                json!(Item::ProviderState {
                    provider: "test".into(),
                    value: json!({})
                }),
            ),
            record(
                3,
                "tool_intent",
                json!(Item::ToolCall {
                    call_id: "a".into(),
                    name: "read".into(),
                    arguments: "{}".into()
                }),
            ),
            record(
                4,
                "tool_intent",
                json!(Item::ToolCall {
                    call_id: "b".into(),
                    name: "read".into(),
                    arguments: "{}".into()
                }),
            ),
            record(
                5,
                "tool_result",
                json!(Item::ToolResult {
                    call_id: "a".into(),
                    result: ToolResult {
                        content: vec![],
                        details: serde_json::Value::Null,
                        artifacts: vec![],
                        text: "x".repeat(90000),
                        exit_code: None,
                        truncated: false,
                        error: None
                    }
                }),
            ),
            record(
                6,
                "tool_result",
                json!(Item::ToolResult {
                    call_id: "b".into(),
                    result: ToolResult {
                        content: vec![],
                        details: serde_json::Value::Null,
                        artifacts: vec![],
                        text: "done".into(),
                        exit_code: None,
                        truncated: false,
                        error: None
                    }
                }),
            ),
        ];
        assert_eq!(retained_cut(&path, 20000).unwrap(), 1);
    }
    #[test]
    fn retained_response_keeps_state_assistant_and_all_tools_as_one_group() {
        let path = vec![
            record(1, "message", json!(text_item("task".into()))),
            record(
                2,
                "provider_state",
                json!(Item::ProviderState {
                    provider: "test".into(),
                    value: json!({ "reasoning": "required state" })
                }),
            ),
            record(
                3,
                "message",
                json!(Item::Message {
                    role: "assistant".into(),
                    content: vec![Block::Text {
                        text: "inspect both files".into()
                    }]
                }),
            ),
            record(
                4,
                "tool_intent",
                json!(Item::ToolCall {
                    call_id: "a".into(),
                    name: "read".into(),
                    arguments: "{}".into()
                }),
            ),
            record(
                5,
                "tool_intent",
                json!(Item::ToolCall {
                    call_id: "b".into(),
                    name: "read".into(),
                    arguments: "{}".into()
                }),
            ),
            record(6, "model_response", json!({ "request_id": "1:2" })),
            record(
                7,
                "tool_result",
                json!(Item::ToolResult {
                    call_id: "a".into(),
                    result: ToolResult {
                        content: vec![],
                        details: serde_json::Value::Null,
                        artifacts: vec![],
                        text: "x".repeat(90000),
                        exit_code: None,
                        truncated: false,
                        error: None
                    }
                }),
            ),
            record(
                8,
                "tool_result",
                json!(Item::ToolResult {
                    call_id: "b".into(),
                    result: ToolResult {
                        content: vec![],
                        details: serde_json::Value::Null,
                        artifacts: vec![],
                        text: "done".into(),
                        exit_code: None,
                        truncated: false,
                        error: None
                    }
                }),
            ),
        ];
        let cut = retained_cut(&path, 20000).unwrap();
        assert_eq!(
            cut, 1,
            "cut must include provider state before assistant text"
        );
        let mut compacted = path.clone();
        compacted.push(record(
            9,
            "compaction",
            json!({ "summary": "earlier task", "first_kept": path[cut].sequence }),
        ));
        let projected = project_records(&compacted).unwrap();
        for kept in &path[1..] {
            if matches!(
                kept.kind.as_str(),
                "provider_state" | "message" | "tool_intent" | "tool_result"
            ) {
                assert!(projected.contains(&decode(kept).unwrap()));
            }
        }
    }
    #[test]
    fn queue_delivery_is_a_user_boundary_for_recent_retention() {
        let path = vec![
            record(1, "message", json!(text_item("old turn".into()))),
            record(
                2,
                "queue_delivered",
                json!({
                    "id": 4,
                    "kind": "steering",
                    "branch": "main",
                    "content": [{ "type": "text", "text": "x".repeat(90000) }],
                }),
            ),
        ];
        assert_eq!(retained_cut(&path, 20000).unwrap(), 1);
    }
    #[test]
    fn recent_budget_does_not_count_raw_history_already_replaced_by_summary() {
        let path = vec![
            record(1, "message", json!(text_item("original task".into()))),
            record(
                2,
                "message",
                json!(Item::Message {
                    role: "assistant".into(),
                    content: vec![Block::Text {
                        text: "x".repeat(100000)
                    }]
                }),
            ),
            record(
                3,
                "compaction",
                json!({ "summary": "small previous summary", "first_kept": 0 }),
            ),
            record(4, "message", json!(text_item("recent small work".into()))),
        ];
        assert_eq!(
            retained_cut(&path, 20000).unwrap(),
            0,
            "no recent prefix exceeds retention budget"
        );
    }
    #[test]
    fn summary_keeps_attachment_reference_but_never_serializes_binary_content() {
        let item = Item::Message {
            role: "user".into(),
            content: vec![
                Block::Image {
                    media_type: "image/png".into(),
                    data: "secretbinary".into(),
                },
                Block::File {
                    name: "report.pdf".into(),
                    media_type: "application/pdf".into(),
                    data: "filebinary".into(),
                },
            ],
        };
        let text = serde_json::to_string(&summary_safe_items(vec![item])).unwrap();
        assert!(!text.contains("secretbinary"));
        assert!(!text.contains("filebinary"));
        assert!(text.contains("report.pdf"));
    }
    #[test]
    fn rebuilding_discards_old_compaction_notes_but_preserves_unrelated_state() {
        let state = |namespace: &str, summary: &str| {
            json!(ExtensionState {
                namespace: namespace.into(),
                version: 1,
                required: false,
                summary: summary.into(),
                references: vec![],
                value: json!({})
            })
        };
        let records = vec![
            record(1, "extension_state", state("manual", "keep")),
            record(
                2,
                "model_request",
                json!({ "request_id": "summary", "purpose": "compaction" }),
            ),
            record(3, "extension_state", state("notes", "old edited summary")),
            record(
                4,
                "compaction",
                json!({ "request_id": "summary", "summary": "old edited summary", "first_kept": 0 }),
            ),
            record(5, "context_rebuild", json!({ "edit_ids": [] })),
        ];
        let states = latest_states(&records).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].namespace, "manual");
    }
}
