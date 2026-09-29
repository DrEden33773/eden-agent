//! Image facts are committed with request preparation; rendering and inspection do not mutate history.
use super::*;
use eden_model_input::{ImageChoice, ImageRecord, InputError, validate_images};
use eden_plugin_sdk::protocol::{context_edit as e, history::active_path, models::ModelTarget};
use std::collections::BTreeMap;

/// Keys refer to the unprocessed document, so omitted blocks cannot shift later identities.
pub(crate) type Choices = BTreeMap<String, BTreeMap<usize, ImageChoice>>;

pub(crate) struct PreparedImages {
    pub document: e::Document,
    pub records: Vec<RecordDraft>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct VersionRecord {
    entry_id: String,
    block_index: usize,
    image: ImageRecord,
}

/// Inspection overlays committed versions without running codecs or making model decisions.
/// Callers must keep the raw document for subsequent operations and stable block positions.
pub(crate) fn view(document: &e::Document, history: &[Record]) -> Result<e::Document, Fault> {
    let mut latest = BTreeMap::new();
    for record in active_path(history)?
        .into_iter()
        .filter(|record| record.kind == "image_version")
    {
        let version: VersionRecord = serde_json::from_value(record.payload)
            .map_err(|error| Fault::new("InvalidImageHistory", "model-input", error.to_string()))?;
        latest.insert(
            (version.entry_id.clone(), version.block_index),
            version.image,
        );
    }
    let mut viewed = document.clone();
    for entry in &mut viewed.entries {
        let Some(content) = content_mut(&mut entry.item) else {
            continue;
        };
        let mut replacement = Vec::with_capacity(content.len());
        for (index, block) in content.iter().enumerate() {
            let prior = latest
                .get(&(entry.id.clone(), index))
                .filter(|image| image.payloads.contains(block));
            if matches!(block, Block::Image { .. })
                && let Some(prior) = prior
            {
                if let Some(sent) = prior.sent().map_err(fault)? {
                    replacement.push(sent.clone());
                }
            } else {
                replacement.push(block.clone());
            }
        }
        *content = replacement;
    }
    Ok(viewed)
}

/// The caller commits these records atomically with the exact prepared request and its revision.
/// `history` is the full store view; only its active ancestry contributes image versions.
pub(crate) fn prepare(
    document: &e::Document,
    target: &Option<ModelTarget>,
    history: &[Record],
    run_id: u64,
    settings: &Settings,
    choices: &Choices,
) -> Result<PreparedImages, Fault> {
    // The legacy Responses route predates model catalogs and already accepts image blocks.
    // Keep that route usable; a known catalog target still owns its explicit capabilities.
    if target.is_none() && settings.image_limits.max_body_bytes.is_some() {
        return Err(Fault::new(
            "UnsupportedImageLimit",
            "model-input",
            "select a catalog model to enforce a serialized request-body limit",
        ));
    }
    let mut legacy = ModelTarget {
        provider: "legacy".into(),
        model: "unspecified".into(),
        ..Default::default()
    };
    legacy.capabilities.images = true;
    let effective_target = target.as_ref().unwrap_or(&legacy);
    let path = active_path(history)?;
    let mut latest = BTreeMap::new();
    for record in path.iter().filter(|record| record.kind == "image_version") {
        let version: VersionRecord = serde_json::from_value(record.payload.clone())
            .map_err(|error| Fault::new("InvalidImageHistory", "model-input", error.to_string()))?;
        latest.insert((version.entry_id.clone(), version.block_index), version);
    }
    let mut prepared = PreparedImages {
        document: document.clone(),
        records: vec![],
    };
    let mut used_choices = BTreeSet::new();
    let mut all_images = vec![];
    for entry in &mut prepared.document.entries {
        let Some(content) = content_mut(&mut entry.item) else {
            continue;
        };
        let mut replacement = Vec::with_capacity(content.len());
        for (block_index, block) in content.iter().enumerate() {
            if !matches!(block, Block::Image { .. }) {
                replacement.push(block.clone());
                continue;
            }
            let explicit = choices
                .get(&entry.id)
                .and_then(|blocks| blocks.get(&block_index))
                .copied();
            if explicit.is_some() {
                used_choices.insert((entry.id.clone(), block_index));
            }
            let key = (entry.id.clone(), block_index);
            // A manual replacement at the same position starts a new source identity.
            let prior = latest
                .get(&key)
                .filter(|version| version.image.payloads.contains(block));
            let mut image = match prior {
                Some(version) => version.image.clone(),
                None => ImageRecord::new(block.clone()).map_err(fault)?,
            };
            if prior.is_none() && !is_new_entry(&entry.id, &path, run_id) {
                // Legacy history already represents sent bytes; target identity is unknown.
                let mut historical = ModelTarget::default();
                historical.capabilities.images = true;
                image = image
                    .prepare(ImageChoice::Preserve, &historical, &Default::default())
                    .map_err(fault)?;
                image.versions[0].explanation =
                    "Historical sent image; original model metadata unavailable".into();
            }
            let choice = explicit.unwrap_or(settings.image_mode);
            let next = image
                .prepare(choice, effective_target, &settings.image_limits)
                .map_err(fault)?;
            if let Some(sent) = next.sent().map_err(fault)? {
                replacement.push(sent.clone());
                all_images.push(sent.clone());
            }
            if prior.is_none_or(|prior| prior.image != next) {
                prepared.records.push(RecordDraft {
                    kind: "image_version".into(),
                    payload: serde_json::to_value(VersionRecord {
                        entry_id: entry.id.clone(),
                        block_index,
                        image: next,
                    })
                    .map_err(|error| {
                        Fault::new("Serialization", "model-input", error.to_string())
                    })?,
                });
            }
        }
        *content = replacement;
    }
    for (entry_id, blocks) in choices {
        for block_index in blocks.keys() {
            if !used_choices.contains(&(entry_id.clone(), *block_index)) {
                return Err(Fault::new(
                    "ImageConflict",
                    "model-input",
                    "selected image is no longer present; refresh its preview",
                ));
            }
        }
    }
    if !all_images.is_empty() {
        validate_images(&all_images, effective_target, &settings.image_limits, None)
            .map_err(fault)?;
    }
    prepared.document.validate()?;
    Ok(prepared)
}

/// Explicit operations use the same preparation path as normal sends; callers retain the
/// original unprocessed document for stable block indexes after omissions.
pub(crate) fn operation(
    document: &e::Document,
    target: &Option<ModelTarget>,
    history: &[Record],
    run_id: u64,
    settings: &Settings,
    choices: &Choices,
) -> Result<PreparedImages, Fault> {
    if choices.is_empty() {
        return Err(Fault::new(
            "InvalidInput",
            "model-input",
            "choose an image and preserve, omit, or re_adapt",
        ));
    }
    if choices
        .values()
        .flat_map(|blocks| blocks.values())
        .any(|choice| *choice == ImageChoice::Auto)
    {
        return Err(Fault::new(
            "InvalidInput",
            "model-input",
            "explicit image operations require preserve, omit, or re_adapt",
        ));
    }
    prepare(document, target, history, run_id, settings, choices)
}

fn content_mut(item: &mut Item) -> Option<&mut Vec<Block>> {
    match item {
        Item::Message { content, .. } => Some(content),
        Item::ToolResult { result, .. } => Some(&mut result.content),
        _ => None,
    }
}
fn is_new_entry(id: &str, path: &[Record], run_id: u64) -> bool {
    if let Some(sequence) = id
        .strip_prefix("record:")
        .and_then(|rest| rest.split(':').next())
        .and_then(|sequence| sequence.parse::<u64>().ok())
    {
        return path
            .iter()
            .any(|record| record.sequence == sequence && record.run_id == run_id);
    }
    path.iter()
        .rev()
        .find(|record| {
            record.kind == "context_edit"
                && record.payload["replacement"]
                    .as_array()
                    .is_some_and(|entries| {
                        entries.iter().any(|entry| entry["id"].as_str() == Some(id))
                    })
        })
        .is_some_and(|record| record.run_id == run_id)
}
fn fault(error: InputError) -> Fault {
    let code = match &error {
        InputError::UnsupportedModel { .. } => "UnsupportedImageModel",
        InputError::LimitExceeded { .. } => "ImageLimitExceeded",
        InputError::InvalidLimit(_) => "InvalidImageLimit",
        InputError::InvalidImage(_) => "InvalidImage",
    };
    Fault::new(code, "model-input", error.to_string())
}

/// The scope joins the blocking worker before library unload, including after root cancellation.
pub(crate) async fn background(
    cx: &CallContext,
    document: &e::Document,
    target: &Option<ModelTarget>,
    history: &[Record],
    settings: &Settings,
    choices: &Choices,
    explicit: bool,
) -> Result<PreparedImages, Fault> {
    let has_images = document.entries.iter().any(|entry| {
        let content = match &entry.item {
            Item::Message { content, .. } => content.as_slice(),
            Item::ToolResult { result, .. } => result.content.as_slice(),
            _ => &[],
        };
        content
            .iter()
            .any(|block| matches!(block, Block::Image { .. }))
    });
    if !has_images {
        return if explicit {
            operation(document, target, history, cx.run_id(), settings, choices)
        } else {
            prepare(document, target, history, cx.run_id(), settings, choices)
        };
    }
    let document = document.clone();
    let target = target.clone();
    let history = history.to_vec();
    let settings = settings.clone();
    let choices = choices.clone();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let run_id = cx.run_id();
    let cancellation = cx.scope.cancellation();
    cx.scope.spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            if cancellation.is_cancelled() {
                return Err(Fault::new(
                    "Cancelled",
                    "model-input",
                    "image preparation cancelled",
                ));
            }
            if explicit {
                operation(&document, &target, &history, run_id, &settings, &choices)
            } else {
                prepare(&document, &target, &history, run_id, &settings, &choices)
            }
        })
        .await
        .map_err(|error| Fault::new("ImageProcessingFailure", "model-input", error.to_string()))?;
        let _ = sender.send(result);
        Ok(())
    })?;
    receiver
        .await
        .map_err(|error| Fault::new("ImageProcessingFailure", "model-input", error.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    // A real 4x2 PNG exercises raw/sent version differences without external fixtures.
    fn image() -> Block {
        Block::Image { media_type: "image/png".into(), data: "iVBORw0KGgoAAAANSUhEUgAAAAQAAAACCAIAAADwyuo0AAAAEElEQVR4nGP4z8AARwzIHABvqgf5gNwAKAAAAABJRU5ErkJggg==".into() }
    }
    fn target() -> Option<ModelTarget> {
        let mut model = ModelTarget {
            provider: "test".into(),
            model: "vision".into(),
            ..Default::default()
        };
        model.capabilities.images = true;
        Some(model)
    }
    fn document() -> e::Document {
        e::Document {
            entries: vec![e::Entry {
                references: vec![],
                id: "record:1:0".into(),
                item: Item::Message {
                    role: "user".into(),
                    content: vec![image()],
                },
            }],
            tools: vec![],
        }
    }
    fn record(sequence: u64, run_id: u64, kind: &str, payload: Value) -> Record {
        Record {
            schema_version: 2,
            session_id: 1,
            sequence,
            run_id,
            parent_id: (sequence > 1).then_some(sequence - 1),
            branch: "main".into(),
            kind: kind.into(),
            payload,
        }
    }
    fn history(run_id: u64) -> Vec<Record> {
        vec![record(
            1,
            run_id,
            "message",
            json!(document().entries[0].item),
        )]
    }
    fn commit(history: &mut Vec<Record>, prepared: &PreparedImages, run_id: u64) {
        for draft in &prepared.records {
            history.push(record(
                history.len() as u64 + 1,
                run_id,
                &draft.kind,
                draft.payload.clone(),
            ));
        }
    }
    #[test]
    fn adapts_current_run_but_preserves_legacy_history_and_old_sent_versions() {
        let settings = Settings::parse(json!({ "images": { "limits": { "max_width": 2 } } }))
            .unwrap()
            .for_target(&target())
            .unwrap();
        let mut records = history(1);
        let adapted = prepare(
            &document(),
            &target(),
            &records,
            1,
            &settings,
            &Choices::new(),
        )
        .unwrap();
        let version: VersionRecord =
            serde_json::from_value(adapted.records[0].payload.clone()).unwrap();
        assert_eq!(version.image.payloads[version.image.original], image());
        assert_eq!(version.image.versions[0].width, 2);
        assert_ne!(version.image.sent().unwrap(), Some(&image()));
        let legacy_error = prepare(
            &document(),
            &target(),
            &records,
            2,
            &settings,
            &Choices::new(),
        )
        .err()
        .unwrap();
        assert_eq!(legacy_error.code, "ImageLimitExceeded");
        commit(&mut records, &adapted, 1);
        let stricter = Settings::parse(json!({ "images": { "limits": { "max_width": 1 } } }))
            .unwrap()
            .for_target(&target())
            .unwrap();
        let error = prepare(
            &document(),
            &target(),
            &records,
            2,
            &stricter,
            &Choices::new(),
        )
        .err()
        .unwrap();
        assert_eq!(error.code, "ImageLimitExceeded");
        let choices = BTreeMap::from([(
            "record:1:0".into(),
            BTreeMap::from([(0, ImageChoice::ReAdapt)]),
        )]);
        let readapted =
            operation(&document(), &target(), &records, 2, &stricter, &choices).unwrap();
        let next: VersionRecord =
            serde_json::from_value(readapted.records[0].payload.clone()).unwrap();
        assert_eq!(next.image.versions.len(), 2);
        assert_eq!(next.image.versions[0], version.image.versions[0]);
        assert_eq!(next.image.versions[1].width, 1);
    }
    #[test]
    fn tool_result_images_share_the_same_validation() {
        let item: Item = serde_json::from_value(json!({
            "type": "tool_result",
            "call_id": "c",
            "result": {
                "text": "",
                "exit_code": null,
                "truncated": false,
                "error": null,
                "content": [image()],
            },
        }))
        .unwrap();
        let doc = e::Document {
            entries: vec![
                e::Entry {
                    references: vec![],
                    id: "call".into(),
                    item: Item::ToolCall {
                        call_id: "c".into(),
                        name: "read".into(),
                        arguments: "{}".into(),
                    },
                },
                e::Entry {
                    references: vec![],
                    id: "record:1:0".into(),
                    item,
                },
            ],
            tools: vec![],
        };
        let error = prepare(
            &doc,
            &Some(ModelTarget::default()),
            &history(1),
            1,
            &Settings::default(),
            &Choices::new(),
        )
        .err()
        .unwrap();
        assert_eq!(error.code, "UnsupportedImageModel");
        assert!(
            prepare(
                &doc,
                &target(),
                &history(1),
                1,
                &Settings::default(),
                &Choices::new()
            )
            .is_ok()
        );
    }
    #[test]
    fn inspection_shows_committed_sent_images_without_reprocessing() {
        let settings = Settings::parse(json!({ "images": { "limits": { "max_width": 2 } } }))
            .unwrap()
            .for_target(&target())
            .unwrap();
        let raw = document();
        let mut records = history(1);
        let prepared = prepare(&raw, &target(), &records, 1, &settings, &Choices::new()).unwrap();
        commit(&mut records, &prepared, 1);
        assert_eq!(
            view(&raw, &records).unwrap().entries[0].item,
            prepared.document.entries[0].item
        );
        assert_eq!(raw.entries[0].item, document().entries[0].item);
        let choices = BTreeMap::from([(
            "record:1:0".into(),
            BTreeMap::from([(0, ImageChoice::Omit)]),
        )]);
        let omitted = operation(&raw, &target(), &records, 2, &settings, &choices).unwrap();
        commit(&mut records, &omitted, 2);
        assert_eq!(
            view(&raw, &records).unwrap().entries[0].item,
            omitted.document.entries[0].item
        );
    }
    #[test]
    fn new_image_metadata_survives_reopen_without_duplicate_versions() {
        let mut history = history(1);
        let prepared = prepare(
            &document(),
            &target(),
            &history,
            1,
            &Settings::default(),
            &Choices::new(),
        )
        .unwrap();
        assert_eq!(prepared.records.len(), 1);
        let image: VersionRecord =
            serde_json::from_value(prepared.records[0].payload.clone()).unwrap();
        assert_eq!(image.image.payloads.len(), 1);
        assert_eq!(image.image.versions.len(), 1);
        commit(&mut history, &prepared, 1);
        let reopened = prepare(
            &document(),
            &target(),
            &history,
            2,
            &Settings::default(),
            &Choices::new(),
        )
        .unwrap();
        assert!(reopened.records.is_empty());
        assert_eq!(
            reopened.document.entries[0].item,
            prepared.document.entries[0].item
        );
    }
    #[test]
    fn historical_images_are_not_silently_omitted_for_text_model() {
        let err = prepare(
            &document(),
            &Some(ModelTarget::default()),
            &history(1),
            2,
            &Settings::default(),
            &Choices::new(),
        )
        .err()
        .unwrap();
        assert_eq!(err.code, "UnsupportedImageModel");
        let choices = BTreeMap::from([(
            "record:1:0".into(),
            BTreeMap::from([(0, ImageChoice::Omit)]),
        )]);
        let prepared = operation(
            &document(),
            &Some(ModelTarget::default()),
            &history(1),
            2,
            &Settings::default(),
            &choices,
        )
        .unwrap();
        let Item::Message { content, .. } = &prepared.document.entries[0].item else {
            panic!()
        };
        assert!(content.is_empty());
        let version: VersionRecord =
            serde_json::from_value(prepared.records[0].payload.clone()).unwrap();
        assert_eq!(version.image.payloads[version.image.original], image());
        assert_eq!(version.image.versions.len(), 1);
    }
    #[test]
    fn omitted_new_image_stays_omitted_on_later_turns() {
        let mut history = history(1);
        let choices = BTreeMap::from([(
            "record:1:0".into(),
            BTreeMap::from([(0, ImageChoice::Omit)]),
        )]);
        let prepared = operation(
            &document(),
            &Some(ModelTarget::default()),
            &history,
            1,
            &Settings::default(),
            &choices,
        )
        .unwrap();
        commit(&mut history, &prepared, 1);
        let reopened = prepare(
            &document(),
            &Some(ModelTarget::default()),
            &history,
            2,
            &Settings::default(),
            &Choices::new(),
        )
        .unwrap();
        assert!(reopened.records.is_empty());
        let Item::Message { content, .. } = &reopened.document.entries[0].item else {
            panic!()
        };
        assert!(content.is_empty());
    }
    #[test]
    fn stale_explicit_selection_fails_instead_of_operating_on_another_image() {
        let choices = BTreeMap::from([(
            "record:1:0".into(),
            BTreeMap::from([(1, ImageChoice::Omit)]),
        )]);
        let error = operation(
            &document(),
            &target(),
            &history(1),
            1,
            &Settings::default(),
            &choices,
        )
        .err()
        .unwrap();
        assert_eq!(error.code, "ImageConflict");
    }
    #[test]
    fn source_on_another_branch_does_not_supply_image_versions() {
        let mut history = history(1);
        let choices = BTreeMap::from([(
            "record:1:0".into(),
            BTreeMap::from([(0, ImageChoice::Omit)]),
        )]);
        let prepared = operation(
            &document(),
            &target(),
            &history,
            1,
            &Settings::default(),
            &choices,
        )
        .unwrap();
        commit(&mut history, &prepared, 1);
        let mut selected = record(
            3,
            2,
            "branch_selected",
            json!({ "target": 1, "branch": "other" }),
        );
        selected.parent_id = Some(1);
        selected.branch = "other".into();
        history.push(selected);
        let other = prepare(
            &document(),
            &target(),
            &history,
            2,
            &Settings::default(),
            &Choices::new(),
        )
        .unwrap();
        let Item::Message { content, .. } = &other.document.entries[0].item else {
            panic!()
        };
        assert_eq!(content, &[image()]);
    }
}
