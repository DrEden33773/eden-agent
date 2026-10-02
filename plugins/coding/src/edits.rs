//! Append-only edits use the storage CAS and never rewrite execution records.
use super::*;
use eden_plugin_sdk::protocol::{context_edit as e, history::active_path};

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, Fault> {
    serde_json::from_value(value)
        .map_err(|error| Fault::new("InvalidContext", "context-edit", error.to_string()))
}
pub(crate) fn apply(
    document: &e::Document,
    records: &[Record],
) -> Result<(e::Document, Vec<u64>), Fault> {
    apply_path(document, &active_path(records)?)
}
pub(crate) fn apply_path(
    document: &e::Document,
    path: &[Record],
) -> Result<(e::Document, Vec<u64>), Fault> {
    let claimed: BTreeSet<u64> = path
        .iter()
        .filter(|record| record.kind == "model_request")
        .flat_map(|record| {
            record.payload["context_edits"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64)
        })
        .collect();
    let rebuild = path
        .iter()
        .rev()
        .find(|record| record.kind == "context_rebuild");
    let rebuild_sequence = rebuild.map_or(0, |record| record.sequence);
    let selected: BTreeSet<u64> = rebuild
        .into_iter()
        .flat_map(|record| {
            record.payload["edit_ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64)
        })
        .collect();
    let absorbed: BTreeSet<u64> = path
        .iter()
        .filter(|record| record.kind == "compaction" && record.sequence > rebuild_sequence)
        .flat_map(|record| {
            record.payload["context_edits"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64)
        })
        .collect();
    let mut effective = document.clone();
    let mut edits = vec![];
    for record in path.iter().filter(|record| record.kind == "context_edit") {
        if absorbed.contains(&record.sequence)
            || (record.sequence < rebuild_sequence && !selected.contains(&record.sequence))
        {
            continue;
        }
        let mut edit: e::Edit = decode(record.payload.clone())?;
        if edit.scope == e::Scope::NextRequest && claimed.contains(&record.sequence) {
            continue;
        }
        if record.sequence < rebuild_sequence {
            // Rebuild explicitly selects these changes even when their old summary/state
            // identities were replaced by original execution records.
            let present: BTreeSet<_> = effective
                .entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect();
            edit.base.retain(|id| present.contains(id.as_str()));
        }
        effective = effective.apply(&edit)?;
        edits.push(record.sequence);
    }
    Ok((effective, edits))
}
pub(crate) async fn service(
    request: e::Request,
    cx: CallContext,
    settings: Settings,
) -> Result<e::Snapshot, Fault> {
    if let e::Request::CheckInput {
        mut input,
        content,
        references,
    } = request
    {
        let before: StoreReply = store_call(&cx, StoreRequest::Read).await?;
        input.records = before.records;
        if input.target.is_none() {
            input.limits = super::model_limits(&cx).await?;
        }
        let limits = input.limits.clone();
        let history = input.records.clone();
        let target = input.target.clone();
        let settings = settings.for_target(&input.target)?;
        let mut snapshot = project_source(input, cx.clone(), settings.clone()).await?;
        snapshot.effective.entries.push(e::Entry {
            id: "inserted:submission-preview".into(),
            item: Item::Message {
                role: "user".into(),
                content,
            },
            references,
        });
        let base = context::estimate(
            &snapshot
                .effective
                .entries
                .iter()
                .map(|entry| entry.item.clone())
                .collect::<Vec<_>>(),
        )
        .saturating_add(
            serde_json::to_string(&snapshot.effective.tools)
                .map_or(0, |text| text.chars().count() as u64 / 4 + 1),
        );
        let remaining = Some(
            snapshot
                .model
                .as_ref()
                .map_or(limits.context_window, |model| model.limits.context_window),
        )
        .filter(|limit| *limit > 0)
        .map_or(u64::MAX, |limit| {
            limit
                .saturating_sub(settings.reserve_tokens)
                .saturating_sub(base)
        });
        snapshot.effective = super::references::materialize(&snapshot.effective, remaining)?;
        snapshot.effective = super::model_input::background(
            &cx,
            &snapshot.effective,
            &target,
            &history,
            &settings,
            &Default::default(),
            super::model_input::Preparation::Draft("inserted:submission-preview".into()),
        )
        .await?
        .document;
        return Ok(snapshot);
    }
    if let e::Request::Images { input, edit } = request {
        return edit_images(input, edit, cx, settings).await;
    }
    if let e::Request::ProjectSource { input } = request {
        let mut snapshot = project_source(input, cx, settings).await?;
        let system = snapshot
            .last_request
            .as_ref()
            .map_or_else(Vec::new, |request| {
                eden_plugin_sdk::protocol::session_reference::system_items(&request.items)
            });
        for entry in &mut snapshot.effective.entries {
            if entry.references.is_empty() {
                continue;
            }
            let blocks = super::references::resolve(&entry.references, &system, u64::MAX)?;
            if let Item::Message { content, .. } = &mut entry.item {
                content.extend(blocks);
            }
            entry.references.clear();
        }
        return Ok(snapshot);
    }
    if let e::Request::Rebuild { input, rebuild } = request {
        return rebuild_context(input, rebuild, cx, settings).await;
    }
    let (mut input, edit) = match request {
        e::Request::Rebuild { .. }
        | e::Request::ProjectSource { .. }
        | e::Request::Images { .. }
        | e::Request::CheckInput { .. } => unreachable!(),
        e::Request::Inspect { input } => (input, None),
        e::Request::Apply { input, edit } => (input, Some(edit)),
    };
    let settings = settings.for_target(&input.target)?;
    if input.target.is_none() {
        input.limits = super::model_limits(&cx).await?;
    }
    let before: StoreReply = store_call(&cx, StoreRequest::Read).await?;
    input.records = before.records.clone();
    input.action = "inspect".into();
    let model: ModelInput = cx.call(CONTEXT, &input).await?;
    let original = decorate(context::document(&input, &cx).await?, &model)?;
    let (effective, edits) = apply(&original, &before.records)?;
    let effective = super::model_input::view(&effective, &before.records)?;
    let revision = e::Revision {
        session_id: before.session_id,
        sequence: before.sequence,
        head: before.active_head,
        branch: before.active_branch.clone(),
    };
    let last_request = active_path(&before.records)?
        .iter()
        .rev()
        .find_map(|record| {
            matches!(
                record.kind.as_str(),
                "model_request" | "model_request_revision"
            )
            .then(|| record.payload.get("input"))
            .flatten()
        })
        .cloned()
        .map(decode)
        .transpose()?;
    let Some(edit) = edit else {
        return Ok(e::Snapshot {
            revision,
            original,
            effective,
            edits,
            last_request,
            policies: settings.policies,
            model: input.target,
            budget: json!(settings.effective_budget),
            image_limits: json!(settings.image_limits),
        });
    };
    if edit.revision.session_id != before.session_id
        || edit.revision.sequence != before.sequence
        || edit.revision.head != before.active_head
        || edit.revision.branch != before.active_branch
    {
        return Err(Fault::new(
            "ContextConflict",
            "context-edit",
            "source revision changed; draft retained, preview again",
        ));
    }
    edit.document.validate()?;
    let previous: std::collections::BTreeMap<_, _> = effective
        .entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect();
    if edit.document.entries.iter().any(|entry| {
        !previous.contains_key(entry.id.as_str()) && !entry.id.starts_with("inserted:")
    }) {
        return Err(Fault::new(
            "InvalidContext",
            "context-edit",
            "new entries require an inserted: identity",
        ));
    }
    if edit.source.trim().is_empty() {
        return Err(Fault::new(
            "InvalidContext",
            "context-edit",
            "edit source is required",
        ));
    }
    let tools_changed = serde_json::to_value(&effective.tools).ok()
        != serde_json::to_value(&edit.document.tools).ok();
    let transaction = e::Edit {
        base: effective
            .entries
            .iter()
            .map(|entry| entry.id.clone())
            .collect(),
        unchanged: edit
            .document
            .entries
            .iter()
            .filter(|entry| {
                previous.get(entry.id.as_str()).is_some_and(|prior| {
                    prior.item == entry.item && prior.references == entry.references
                })
            })
            .map(|entry| entry.id.clone())
            .collect(),
        replacement: edit.document.entries.clone(),
        tools: tools_changed.then(|| edit.document.tools.clone()),
        scope: edit.scope,
        source: edit.source,
    };
    let committed: StoreReply = store_call(
        &cx,
        StoreRequest::AppendChecked {
            new_branch: None,
            run_id: cx.run_id(),
            session_id: before.session_id,
            sequence: before.sequence,
            head: before.active_head,
            branch: before.active_branch,
            entries: vec![RecordDraft {
                kind: "context_edit".into(),
                payload: json!(transaction),
            }],
        },
    )
    .await?;
    cx.invalidate_snapshot().await?;
    cx.emit(
        "committed",
        json!({ "sequence": committed.sequence, "kind": "context_edit" }),
    )?;
    let mut edits = edits;
    edits.push(committed.sequence);
    Ok(e::Snapshot {
        revision: e::Revision {
            session_id: committed.session_id,
            sequence: committed.sequence,
            head: committed.active_head,
            branch: committed.active_branch,
        },
        original,
        effective: edit.document,
        edits,
        last_request,
        policies: settings.policies,
        model: input.target,
        budget: json!(settings.effective_budget),
        image_limits: json!(settings.image_limits),
    })
}

pub(crate) async fn prepare(
    input: &ContextInput,
    mut model: ModelInput,
    cx: &CallContext,
    settings: &Settings,
) -> Result<e::Prepared, Fault> {
    let before: StoreReply = store_call(cx, StoreRequest::Read).await?;
    if before
        .records
        .iter()
        .skip(input.records.len())
        .any(|record| {
            !matches!(
                record.kind.as_str(),
                "model_request"
                    | "model_response"
                    | "model_attempt"
                    | "extension_state"
                    | "compaction"
                    | "branch_summary"
            )
        })
    {
        return Err(Fault::new(
            "RequestPreparationConflict",
            "context-edit",
            "input changed during request preparation",
        ));
    }
    let mut input = input.clone();
    input.records = before.records.clone();
    let document = decorate(context::document(&input, cx).await?, &model)?;
    let (effective, edits) = apply(&document, &before.records)?;
    let references = effective
        .entries
        .iter()
        .any(|entry| !entry.references.is_empty());
    let base = context::estimate(
        &effective
            .entries
            .iter()
            .map(|entry| entry.item.clone())
            .collect::<Vec<_>>(),
    )
    .saturating_add(
        serde_json::to_string(&effective.tools)
            .map_or(0, |text| text.chars().count() as u64 / 4 + 1),
    );
    let budget = Some(
        model
            .target
            .as_ref()
            .map_or(input.limits.context_window, |target| {
                target.limits.context_window
            }),
    )
    .filter(|limit| *limit > 0)
    .map_or(u64::MAX, |limit| {
        limit
            .saturating_sub(settings.reserve_tokens)
            .saturating_sub(base)
    });
    let (effective, prompt_cache) =
        super::references::materialize_cached(&effective, budget, &before.records, &edits)?;
    let images = super::model_input::background(
        cx,
        &effective,
        &model.target,
        &before.records,
        settings,
        &Default::default(),
        super::model_input::Preparation::Request,
    )
    .await?;
    images.document.replace_input(&mut model);
    if let Some(target) = &mut model.target {
        target.compat["eden_image_limits"] = json!(settings.image_limits);
    }
    e::validate(&model)?;
    Ok(e::Prepared {
        input: model,
        edits,
        image_records: images.records,
        references,
        prompt_cache,
        revision: e::Revision {
            session_id: before.session_id,
            sequence: before.sequence,
            head: before.active_head,
            branch: before.active_branch,
        },
    })
}

pub(crate) fn pending_temporary(records: &[Record]) -> Result<bool, Fault> {
    let path = active_path(records)?;
    let claimed: BTreeSet<u64> = path
        .iter()
        .filter(|record| record.kind == "model_request")
        .flat_map(|record| {
            record.payload["context_edits"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64)
        })
        .collect();
    Ok(path.iter().any(|record| {
        record.kind == "context_edit"
            && record.payload["scope"] == "next_request"
            && !claimed.contains(&record.sequence)
    }))
}

async fn rebuild_context(
    mut input: ContextInput,
    rebuild: e::Rebuild,
    cx: CallContext,
    settings: Settings,
) -> Result<e::Snapshot, Fault> {
    let before: StoreReply = store_call(&cx, StoreRequest::Read).await?;
    let path = active_path(&before.records)?;
    if rebuild.edit_ids.iter().any(|id| {
        !path.iter().any(|record| {
            record.sequence == *id
                && record.kind == "context_edit"
                && record.payload["scope"] == "branch"
        })
    }) {
        return Err(Fault::new(
            "InvalidContext",
            "context-edit",
            "rebuild accepts only persistent edits from the selected ancestry",
        ));
    }
    let mut candidate = before.records.clone();
    candidate.push(Record {
        schema_version: 2,
        session_id: before.session_id,
        sequence: before.sequence + 1,
        run_id: cx.run_id(),
        parent_id: before.active_head,
        branch: rebuild.branch.clone(),
        kind: "context_rebuild".into(),
        payload: json!({ "edit_ids": rebuild.edit_ids, "source_head": before.active_head }),
    });
    input.records = candidate;
    let original = context::document(&input, &cx).await?;
    let (effective, edits) = apply(&original, &input.records)?;
    effective.validate()?;
    let committed: StoreReply = store_call(
        &cx,
        StoreRequest::AppendChecked {
            run_id: cx.run_id(),
            session_id: rebuild.revision.session_id,
            sequence: rebuild.revision.sequence,
            head: rebuild.revision.head,
            branch: rebuild.revision.branch,
            new_branch: Some(rebuild.branch),
            entries: vec![RecordDraft {
                kind: "context_rebuild".into(),
                payload: json!({ "edit_ids": rebuild.edit_ids, "source_head": before.active_head }),
            }],
        },
    )
    .await?;
    cx.invalidate_snapshot().await?;
    cx.emit(
        "committed",
        json!({ "sequence": committed.sequence, "kind": "context_rebuild" }),
    )?;
    Ok(e::Snapshot {
        revision: e::Revision {
            session_id: committed.session_id,
            sequence: committed.sequence,
            head: committed.active_head,
            branch: committed.active_branch,
        },
        original,
        effective,
        edits,
        last_request: None,
        policies: settings.policies,
        model: input.target,
        budget: json!(settings.effective_budget),
        image_limits: json!(settings.image_limits),
    })
}

async fn project_source(
    input: ContextInput,
    cx: CallContext,
    settings: Settings,
) -> Result<e::Snapshot, Fault> {
    eden_plugin_sdk::protocol::history::validate_records(&input.records)?;
    let (head, branch) = eden_plugin_sdk::protocol::history::branch_state(&input.records)?;
    let original = context::document(&input, &cx).await?;
    let (effective, edits) = apply(&original, &input.records)?;
    let effective = super::model_input::view(&effective, &input.records)?;
    let path = active_path(&input.records)?;
    let last_request = path
        .iter()
        .rev()
        .filter(|record| {
            matches!(
                record.kind.as_str(),
                "model_request" | "model_request_revision"
            )
        })
        .find_map(|record| record.payload.get("input"))
        .cloned()
        .map(decode)
        .transpose()?;
    let settings = settings.for_target(&input.target)?;
    Ok(e::Snapshot {
        revision: e::Revision {
            session_id: input.records.first().map_or(0, |record| record.session_id),
            sequence: input.records.len() as u64,
            head,
            branch,
        },
        original,
        effective,
        edits,
        last_request,
        policies: settings.policies,
        model: input.target,
        budget: json!(settings.effective_budget),
        image_limits: json!(settings.image_limits),
    })
}

async fn edit_images(
    mut input: ContextInput,
    edit: e::ImageEdit,
    cx: CallContext,
    settings: Settings,
) -> Result<e::Snapshot, Fault> {
    let before: StoreReply = store_call(&cx, StoreRequest::Read).await?;
    if edit.revision.session_id != before.session_id
        || edit.revision.sequence != before.sequence
        || edit.revision.head != before.active_head
        || edit.revision.branch != before.active_branch
    {
        return Err(Fault::new(
            "ContextConflict",
            "model-input",
            "image preview changed; refresh before applying",
        ));
    }
    input.records = before.records.clone();
    let original = context::document(&input, &cx).await?;
    let (effective, _) = apply(&original, &before.records)?;
    let choices = edit
        .choices
        .into_iter()
        .map(|(id, blocks)| {
            (
                id,
                blocks
                    .into_iter()
                    .map(|(index, action)| {
                        (
                            index,
                            match action {
                                e::ImageAction::Preserve => eden_model_input::ImageChoice::Preserve,
                                e::ImageAction::Omit => eden_model_input::ImageChoice::Omit,
                                e::ImageAction::ReAdapt => eden_model_input::ImageChoice::ReAdapt,
                            },
                        )
                    })
                    .collect(),
            )
        })
        .collect();
    let settings = settings.for_target(&input.target)?;
    let prepared = super::model_input::background(
        &cx,
        &effective,
        &input.target,
        &before.records,
        &settings,
        &choices,
        super::model_input::Preparation::Operation,
    )
    .await?;
    if !prepared.records.is_empty() {
        let committed: StoreReply = store_call(
            &cx,
            StoreRequest::AppendChecked {
                run_id: cx.run_id(),
                session_id: before.session_id,
                sequence: before.sequence,
                head: before.active_head,
                branch: before.active_branch,
                new_branch: None,
                entries: prepared.records,
            },
        )
        .await?;
        cx.invalidate_snapshot().await?;
        cx.emit(
            "committed",
            json!({ "sequence": committed.sequence, "kind": "image_version" }),
        )?;
        input.records = committed.records;
    }
    project_source(input, cx, settings).await
}

fn decorate(original: e::Document, model: &ModelInput) -> Result<e::Document, Fault> {
    if original
        .entries
        .iter()
        .map(|entry| &entry.item)
        .eq(model.items.iter())
    {
        return Ok(e::Document {
            entries: original.entries,
            tools: model.tools.clone(),
        });
    }
    // A replacement context can supply synthesized structures. Only this uncommon path
    // derives content identities; normal history projection keeps its source-node identities.
    use sha2::{Digest, Sha256};
    let mut originals =
        std::collections::BTreeMap::<String, std::collections::VecDeque<e::Entry>>::new();
    for entry in original.entries {
        let key = serde_json::to_string(&entry.item)
            .map_err(|error| Fault::new("InvalidContext", "context-edit", error.to_string()))?;
        originals.entry(key).or_default().push_back(entry);
    }
    let mut occurrences = std::collections::BTreeMap::new();
    let mut entries = Vec::new();
    for item in &model.items {
        let key = serde_json::to_string(item)
            .map_err(|error| Fault::new("InvalidContext", "context-edit", error.to_string()))?;
        if let Some(entry) = originals
            .get_mut(&key)
            .and_then(|entries| entries.pop_front())
        {
            entries.push(entry);
        } else {
            let digest = format!("{:x}", Sha256::digest(key.as_bytes()));
            let index = occurrences.entry(digest.clone()).or_insert(0);
            entries.push(e::Entry {
                id: format!("projection:{digest}:{index}"),
                item: item.clone(),
                references: vec![],
            });
            *index += 1;
        }
    }
    Ok(e::Document {
        entries,
        tools: model.tools.clone(),
    })
}

pub(crate) async fn recover_request(
    cx: &CallContext,
    input: &ContextInput,
    model: &ModelInput,
    applied: &[u64],
    settings: &Settings,
    request_id: &str,
    first_sequence: u64,
) -> Result<ModelInput, Fault> {
    let retry = if applied.is_empty() {
        let history: StoreReply = store_call(cx, StoreRequest::Read).await?;
        reject_late_changes(&history.records, first_sequence)?;
        let mut recovery = input.clone();
        recovery.action = "overflow".into();
        recovery.records = history.records;
        let compacted: ModelInput = cx.call(CONTEXT, &recovery).await?;
        let after: StoreReply = store_call(cx, StoreRequest::Read).await?;
        reject_late_changes(&after.records, first_sequence)?;
        recovery.records = after.records;
        // A context strategy returns original image blocks. Every actual attempt must still
        // use committed image versions and the frozen model-specific input limits.
        prepare(&recovery, compacted, cx, settings).await?
    } else {
        let compacted = context::recover_edited(input, model, cx, settings).await?;
        let history: StoreReply = store_call(cx, StoreRequest::Read).await?;
        e::Prepared {
            input: compacted,
            revision: e::Revision {
                session_id: history.session_id,
                sequence: history.sequence,
                head: history.active_head,
                branch: history.active_branch,
            },
            edits: applied.to_vec(),
            image_records: vec![],
            references: false,
            prompt_cache: Value::Null,
        }
    };
    let mut records = retry.image_records;
    records.push(RecordDraft {
        kind: "model_request_revision".into(),
        payload: json!({
            "request_id": request_id,
            "input": retry.input,
            "prompt_cache": retry.prompt_cache,
        }),
    });
    let committed: StoreReply = store_call(
        cx,
        StoreRequest::AppendChecked {
            run_id: cx.run_id(),
            session_id: retry.revision.session_id,
            sequence: retry.revision.sequence,
            head: retry.revision.head,
            branch: retry.revision.branch,
            new_branch: None,
            entries: records,
        },
    )
    .await?;
    cx.emit(
        "committed",
        json!({ "sequence": committed.sequence, "kind": "model_request_revision" }),
    )?;
    Ok(retry.input)
}
fn reject_late_changes(records: &[Record], first_sequence: u64) -> Result<(), Fault> {
    if records.iter().any(|record| {
        record.sequence > first_sequence
            && matches!(
                record.kind.as_str(),
                "context_edit"
                    | "context_rebuild"
                    | "image_version"
                    | "message"
                    | "tool_intent"
                    | "tool_result"
                    | "provider_state"
                    | "queue_delivered"
                    | "user_shell"
            )
    }) {
        return Err(Fault::new(
            "ContextConflict",
            "context-edit",
            "context changed after this request started; retry as a new request to use the \
             accepted change",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(id: &str, text: &str) -> e::Entry {
        e::Entry {
            id: id.into(),
            references: vec![],
            item: Item::Message {
                role: "user".into(),
                content: vec![Block::Text { text: text.into() }],
            },
        }
    }
    fn record(
        sequence: u64,
        parent: Option<u64>,
        branch: &str,
        kind: &str,
        payload: Value,
    ) -> Record {
        Record {
            schema_version: 2,
            session_id: 1,
            sequence,
            parent_id: parent,
            branch: branch.into(),
            run_id: 1,
            kind: kind.into(),
            payload,
        }
    }
    fn edit(scope: e::Scope) -> e::Edit {
        e::Edit {
            base: vec!["record:1:0".into()],
            replacement: vec![entry("record:1:0", "edited")],
            unchanged: vec![],
            tools: None,
            scope,
            source: "test".into(),
        }
    }
    #[test]
    fn persisted_edit_preserves_new_descendants_and_other_branch() {
        let original = e::Document {
            entries: vec![entry("record:1:0", "original"), entry("record:3:0", "new")],
            tools: vec![],
        };
        let mut records = vec![
            record(1, None, "main", "message", json!(original.entries[0].item)),
            record(
                2,
                Some(1),
                "main",
                "context_edit",
                json!(edit(e::Scope::Branch)),
            ),
        ];
        let (effective, ids) = apply(&original, &records).unwrap();
        assert_eq!(ids, vec![2]);
        assert_eq!(effective.entries[0].item, entry("", "edited").item);
        assert_eq!(effective.entries[1].item, entry("", "new").item);
        records.push(record(
            3,
            Some(1),
            "other",
            "branch_selected",
            json!({ "target": 1, "branch": "other" }),
        ));
        assert!(apply(&original, &records).unwrap().1.is_empty());
        assert_eq!(original.entries[0].item, entry("", "original").item);
    }
    #[test]
    fn temporary_edit_is_claimed_by_request_not_inspection_or_failure() {
        let original = e::Document {
            entries: vec![entry("record:1:0", "original")],
            tools: vec![],
        };
        let mut records = vec![
            record(1, None, "main", "message", json!(original.entries[0].item)),
            record(
                2,
                Some(1),
                "main",
                "context_edit",
                json!(edit(e::Scope::NextRequest)),
            ),
        ];
        assert_eq!(apply(&original, &records).unwrap().1, vec![2]);
        assert_eq!(apply(&original, &records).unwrap().1, vec![2]);
        records.push(record(
            3,
            Some(2),
            "main",
            "model_request",
            json!({ "context_edits": [2] }),
        ));
        records.push(record(4, Some(3), "main", "run_failed", json!({})));
        assert!(apply(&original, &records).unwrap().1.is_empty());
    }
    #[test]
    fn untouched_entries_use_current_facts_and_modified_missing_sources_fail() {
        let original = e::Document {
            entries: vec![entry("record:1:0", "fresh")],
            tools: vec![],
        };
        let mut change = edit(e::Scope::Branch);
        change.unchanged.push("record:1:0".into());
        assert_eq!(
            original.apply(&change).unwrap().entries[0].item,
            entry("", "fresh").item
        );
        change.unchanged.clear();
        let empty = e::Document {
            entries: vec![],
            tools: vec![],
        };
        assert_eq!(empty.apply(&change).unwrap_err().code, "ContextConflict");
    }
}
