use super::*;
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
    let compact = path.iter().rev().find(|record| record.kind == "compaction");
    let first = compact
        .and_then(|record| record.payload.get("first_kept"))
        .and_then(Value::as_u64);
    let states = queue::statuses(records);
    let mut items = vec![];
    if let Some(record) = compact {
        items.push(text_item(format!(
            "Previous context summary:\n{}",
            record.payload["summary"].as_str().unwrap_or_default()
        )));
        add_uncertainties(&mut items, &record.payload);
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
            "branch_summary" => {
                items.push(text_item(format!(
                    "Explicit context carried from another branch:\n{}",
                    record.payload["summary"].as_str().unwrap_or_default()
                )));
                add_uncertainties(&mut items, &record.payload);
            }
            _ => {}
        }
    }
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
    Ok(items
        .into_iter()
        .map(|item| match item {
            Item::ToolCall { ref call_id, .. } if !completed.contains(call_id) => {
                text_item(format!(
                    concat!(
                        "Historical tool call {call_id} was interrupted; result and external effects ",
                        "are unknown. It was not replayed.",
                    ),
                    call_id = call_id,
                ))
            }
            other => other,
        })
        .collect())
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
fn estimate(items: &[Item]) -> u64 {
    items
        .iter()
        .map(|item| serde_json::to_string(item).map_or(0, |s| s.chars().count() as u64 / 4 + 1))
        .sum()
}
fn retained_cut(path: &[Record], keep_recent_tokens: u64) -> Result<usize, Fault> {
    // Keep about 20k recent tokens. A cut may only precede a complete assistant/tool round.
    let first = path
        .iter()
        .rev()
        .find(|record| record.kind == "compaction")
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
        ) {
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
            || (record.kind == "message"
                && matches!(decode(record)?,Item::Message{role,..} if role=="user"));
        let round = record.kind == "provider_state"
            || (record.kind == "message"
                && matches!(decode(record)?,Item::Message{role,..} if role=="assistant"))
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
                    && matches!(decode(previous)?,Item::Message{role,..} if role=="assistant"));
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
                                concat!(
                                    "[File attachment {name} ({media_type}); preserved in original ",
                                    "record]",
                                ),
                                name = name,
                                media_type = media_type,
                            ),
                        },
                        other => other,
                    })
                    .collect(),
            },
            Item::ProviderState { provider, .. } => text_item(format!(
                "[Opaque {provider} provider state preserved in original record]"
            )),
            other => other,
        })
        .collect()
}
fn latest_states(records: &[Record]) -> Result<Vec<ExtensionState>, Fault> {
    let states: Vec<ExtensionState> = records
        .iter()
        .filter(|r| r.kind == "extension_state")
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
pub(crate) async fn context(
    mut input: ContextInput,
    cx: CallContext,
    settings: Settings,
) -> Result<ModelInput, Fault> {
    if !["", "project", "compact", "branch_summary"].contains(&input.action.as_str()) {
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
    if input.limits.context_window == 0 {
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
    let extension_items = interpreted(&path, &cx).await?;
    let mut projected = project_path(&path, &input.records)?;
    projected.extend(input.items.clone());
    let threshold = settings.compaction_enabled
        && input.limits.context_window > 0
        && estimate(&projected)
            > input
                .limits
                .context_window
                .saturating_sub(settings.reserve_tokens);
    if input.action == "compact"
        || input.action == "branch_summary"
        || ((input.action.is_empty() || input.action == "project") && threshold)
    {
        let path = if input.action == "branch_summary" {
            input.records.clone()
        } else {
            active_path(&input.records)?
        };
        let mut cut = compaction_cut(&path, &input.action, &settings)?;
        if input.action != "branch_summary" {
            let statuses = queue::statuses(&input.records);
            if let Some(index) = path.iter().position(|record| {
                record.kind == "queue_delivered"
                    && statuses
                        .get(&record.payload["id"].as_u64().unwrap_or(0))
                        .is_some_and(|(state, last)| {
                            *state == "queue_delivered" && last.sequence == record.sequence
                        })
            }) {
                cut = cut.min(index);
            }
        }
        if cut == 0 && input.action == "compact" {
            cx.emit(
                "compaction_skipped",
                json!({ "reason": "no_older_context" }),
            )?;
        }
        if cut > 0 {
            let split = cut < path.len()
                && !(path[cut].kind == "message"
                    && matches!(decode(&path[cut]),Ok(Item::Message{role,..}) if role=="user"))
                && path[..cut].iter().any(|record| {
                    record.kind == "message"
                        && matches!(decode(record),Ok(Item::Message{role,..}) if role=="user")
                });
            let allowance = settings.summary_allowance(split, input.limits.max_output_tokens);
            let mut summary_items = vec![Item::Message {
                role: "system".into(),
                content: vec![Block::Text {
                    text: format!(
                        concat!(
                            "Summarize this coding conversation for continuation. Use headings: Goal, ",
                            "Constraints, Progress (Done/In Progress/Blocked), Key Decisions, Next ",
                            "Steps, Critical Context. Preserve user requirements, exact paths/functions/errors, ",
                            "failed checks, pending work, and unknown external tool effects. Update ",
                            "previous summaries rather than discarding them. Do not continue the ",
                            "task. {}",
                        ),
                        input.instructions
                    ),
                }],
            }];
            let summary_projection =
                summary_safe_items(summary_prefix(&path, cut, &input.records)?);
            summary_items.push(text_item(format!(
                "Conversation to summarize (attachments remain in raw history):\n{}",
                serde_json::to_string(&summary_projection).map_err(|e| Fault::new(
                    "InvalidInput",
                    "context",
                    e.to_string()
                ))?
            )));
            summary_items.extend(extension_items.clone());
            let reply = super::provider_retry(
                &cx,
                &ModelInput {
                    max_output_tokens: Some(allowance),
                    items: summary_items,
                    tools: vec![],
                },
                &settings,
            )
            .await?;
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
            let (read, modified, uncertainties) = evidence(&path);
            let kind = if input.action == "branch_summary" {
                "branch_summary"
            } else {
                "compaction"
            };
            let mut payload = json!({
                "summary": summary,
                "read_files": read,
                "modified_files": modified,
                "uncertainties": uncertainties,
            });
            if kind == "compaction" {
                payload["first_kept"] = json!(path.get(cut).map_or(0, |r| r.sequence));
                payload["source_ids"] =
                    json!(path[..cut].iter().map(|r| r.sequence).collect::<Vec<_>>());
            } else {
                payload["origin_session"] =
                    json!(path.first().map_or(cx.session_id(), |r| r.session_id));
                payload["origin_ids"] = json!(path.iter().map(|r| r.sequence).collect::<Vec<_>>());
            }
            append(&cx, kind, payload).await?;
            let history: StoreReply = cx.call(STORE, &StoreRequest::Read).await?;
            projected = project_records(&history.records)?;
        }
    }
    let mut system = format!(
        concat!(
            "You are eden, a coding assistant. Work in {}. Use the available tools to inspect, change ",
            "and verify the project. Report observed results accurately. Tool failures are evidence ",
            "to address; do not claim unexecuted checks passed.",
        ),
        input.cwd
    );
    if let Some(resources) = &input.resources {
        if let Some(replacement) = &resources.system {
            system.clone_from(replacement);
        }
        system.push_str("\n\n");
        system.push_str(&resources.append_system);
        system.push_str(&resources.instructions);
        for skill in resources
            .skills
            .iter()
            .filter(|skill| skill.model_invocable)
        {
            system.push_str(&format!(
                "\nAvailable skill {}: {}. Use the skill tool to load its instructions on demand.",
                skill.name, skill.description
            ));
        }
    }
    let mut items = vec![Item::Message {
        role: "system".into(),
        content: vec![Block::Text { text: system }],
    }];
    items.extend(projected);
    items.extend(extension_items);
    Ok(ModelInput {
        max_output_tokens: None,
        items,
        tools: input.tools.unwrap_or_else(tools),
    })
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
            "a short transcript must have no compactable prefix, not one that replaces recent messages"
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
}
