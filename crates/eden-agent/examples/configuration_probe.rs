//! Installed native configuration commits, writer reopening and failed-commit recovery.
use eden_agent::{
    Session, SessionOptions,
    configuration::{ApplyMode, Change, Inspection, Receipt, Status},
};
use eden_protocol::{Composition, coding::Record};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

type ProbeResult<T> = Result<T, Box<dyn std::error::Error>>;

fn generation(inspection: &Inspection, instance: &str) -> u64 {
    inspection
        .instances
        .iter()
        .find(|candidate| candidate.id == instance)
        .and_then(|candidate| candidate.generation)
        .expect("native instance has a generation")
}

fn effective(inspection: &Inspection, instance: &str) -> Value {
    inspection
        .instances
        .iter()
        .find(|candidate| candidate.id == instance)
        .expect("native instance is inspectable")
        .effective
        .clone()
}

fn change(instance: &str, revision: u64, patch: Value) -> Change {
    Change {
        instance: instance.into(),
        revision,
        patch,
        replacement: None,
    }
}

fn latest_binding(records: &[Record]) -> &Value {
    &records
        .iter()
        .rev()
        .find(|record| record.kind == "composition_lock")
        .expect("session has a durable composition binding")
        .payload
}

fn assert_commit(records: &[Record], receipt: &Receipt) {
    let position = records
        .iter()
        .rposition(|record| record.kind == "configuration_commit")
        .expect("successful configuration has a durable commit");
    let commit = &records[position];
    assert_eq!(commit.payload["receipt"], json!(receipt));
    assert_eq!(commit.payload["change"]["instance"], receipt.instance);
    assert_eq!(records[position + 1].kind, "composition_lock");
    assert_eq!(records[position + 1].parent_id, Some(commit.sequence));
}

fn assert_atomic_commit(path: &Path, receipt: &Receipt) -> ProbeResult<()> {
    let lines = std::fs::read_to_string(path)?;
    let frames = lines
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    assert!(
        frames.iter().any(
            |frame| frame["transaction"].as_array().is_some_and(|records| {
                records.windows(2).any(|pair| {
                    pair[0]["kind"] == "configuration_commit"
                        && pair[0]["payload"]["receipt"]["operation"] == receipt.operation
                        && pair[1]["kind"] == "composition_lock"
                })
            })
        ),
        "configuration receipt and binding must share one durable transaction frame"
    );
    Ok(())
}

async fn apply(session: &Session, change: Change) -> ProbeResult<Receipt> {
    let operation = session.apply_configuration(change, ApplyMode::Wait).await?;
    Ok(session.wait_configuration(operation).await?)
}

async fn committed_configuration(path: &Path, scratch: &Path) -> ProbeResult<Value> {
    let options = SessionOptions {
        cwd: scratch.to_owned(),
        history: Some(scratch.join("committed.jsonl")),
    };
    let session = Session::open_with(path, options.clone()).await?;
    let original = session.inspect_configuration().await?;
    let store = generation(&original, "local-history");
    let model = generation(&original, "model-access");
    let search = generation(&original, "search");
    let receipt = apply(
        &session,
        change(
            "search",
            original.revision,
            json!({ "allow_broad_scan": true }),
        ),
    )
    .await?;
    assert_eq!(receipt.status, Status::Applied);
    let updated = session.inspect_configuration().await?;
    assert_ne!(generation(&updated, "search"), search);
    assert_eq!(generation(&updated, "local-history"), store);
    assert_eq!(generation(&updated, "model-access"), model);
    assert_eq!(effective(&updated, "search")["allow_broad_scan"], true);
    assert_commit(&session.history().await?, &receipt);
    assert_atomic_commit(options.history.as_ref().unwrap(), &receipt)?;
    session.shutdown().await?;

    // The caller still supplies the original composition; committed overrides and receipts replay.
    let session = Session::open_with(path, options.clone()).await?;
    let restored = session.inspect_configuration().await?;
    assert_eq!(restored.revision, receipt.revision);
    assert_eq!(effective(&restored, "search")["allow_broad_scan"], true);
    assert_eq!(
        json!(session.configuration_operation(receipt.operation)?),
        json!(receipt)
    );
    let model = generation(&restored, "model-access");
    let search = generation(&restored, "search");
    let store = generation(&restored, "local-history");
    let before = session.history().await?;
    let store_receipt = apply(
        &session,
        change(
            "local-history",
            restored.revision,
            json!({ "configuration_probe": "reopen-writer" }),
        ),
    )
    .await?;
    assert_eq!(store_receipt.status, Status::Applied);
    let updated = session.inspect_configuration().await?;
    assert_ne!(generation(&updated, "local-history"), store);
    assert_eq!(generation(&updated, "model-access"), model);
    assert_eq!(generation(&updated, "search"), search);
    let after = session.history().await?;
    assert_eq!(json!(&after[..before.len()]), json!(before));
    assert_commit(&after, &store_receipt);
    assert_atomic_commit(options.history.as_ref().unwrap(), &store_receipt)?;

    // A distinct immutable path is committed; reopening must not need the transient input file.
    let mut replacement: Composition = serde_json::from_slice(&std::fs::read(path)?)?;
    let mut new_library = None;
    for manifest in &mut replacement.packages {
        let source = std::fs::canonicalize(
            path.parent()
                .unwrap_or(Path::new("."))
                .join(&manifest.library),
        )?;
        manifest.library = source.to_string_lossy().into_owned();
        if manifest.descriptor.package == "search" {
            let destination = scratch.join(format!(
                "replacement-{}",
                source.file_name().unwrap().to_string_lossy()
            ));
            std::fs::copy(&source, &destination)?;
            manifest.library = destination.to_string_lossy().into_owned();
            new_library = Some(destination);
        }
    }
    let new_library = std::fs::canonicalize(new_library.ok_or("search library missing")?)?;
    let replacement_path = scratch.join("replacement-composition.json");
    std::fs::write(&replacement_path, serde_json::to_vec(&replacement)?)?;
    let mut replacement_change = change("search", updated.revision, json!({}));
    replacement_change.replacement = Some(replacement_path.clone());
    let replacement_receipt = apply(&session, replacement_change).await?;
    assert_eq!(replacement_receipt.status, Status::Applied);
    let updated = session.inspect_configuration().await?;
    assert_ne!(generation(&updated, "search"), search);
    assert_eq!(generation(&updated, "model-access"), model);
    let records = session.history().await?;
    assert_commit(&records, &replacement_receipt);
    assert_atomic_commit(options.history.as_ref().unwrap(), &replacement_receipt)?;
    let binding = latest_binding(&records).clone();
    assert!(
        binding["library_locations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value
                .as_str()
                .is_some_and(|value| Path::new(value) == new_library))
    );
    let commit = records
        .iter()
        .rev()
        .find(|record| record.kind == "configuration_commit")
        .unwrap();
    assert_eq!(
        Path::new(
            commit.payload["replacement_package"]["library"]
                .as_str()
                .unwrap()
        ),
        new_library
    );
    session.shutdown().await?;
    std::fs::remove_file(replacement_path)?;
    let reopened = Session::open_with(path, options).await?;
    let replayed = reopened.inspect_configuration().await?;
    assert_eq!(replayed.revision, replacement_receipt.revision);
    assert_eq!(effective(&replayed, "search")["allow_broad_scan"], true);
    assert_eq!(
        effective(&replayed, "local-history")["configuration_probe"],
        "reopen-writer"
    );
    assert_eq!(
        json!(reopened.configuration_operation(replacement_receipt.operation)?),
        json!(replacement_receipt)
    );
    assert_eq!(latest_binding(&reopened.history().await?), &binding);
    reopened.shutdown().await?;
    Ok(json!({
        "target_native_generation_changed": true,
        "unrelated_store_and_model_retained": true,
        "configuration_commit_and_binding_agree": true,
        "store_restart_reopens_writer_and_preserves_records": true,
        "old_composition_replays_override_and_receipts": true,
        "immutable_library_replacement_reopens_without_input_manifest": true,
    }))
}

async fn rejected_commit(path: &Path, scratch: &Path) -> ProbeResult<Value> {
    let options = SessionOptions {
        cwd: scratch.to_owned(),
        history: Some(scratch.join("rejected.jsonl")),
    };
    let session = Session::open_with(path, options.clone()).await?;
    let original = session.inspect_configuration().await?;
    let search = generation(&original, "search");
    let store = generation(&original, "coding-replacements");
    let model = generation(&original, "model-access");
    let original_config = effective(&original, "search");
    let original_records = session.history().await?;
    let binding = latest_binding(&original_records).clone();
    let receipt = apply(
        &session,
        change(
            "search",
            original.revision,
            json!({ "allow_broad_scan": true }),
        ),
    )
    .await?;
    assert_eq!(receipt.status, Status::Restored);
    assert_eq!(receipt.error.as_ref().unwrap().code, "PersistenceFailure");
    assert!(receipt.recovery_error.is_none());
    let restored = session.inspect_configuration().await?;
    assert_ne!(generation(&restored, "search"), search);
    assert_eq!(generation(&restored, "coding-replacements"), store);
    assert_eq!(generation(&restored, "model-access"), model);
    assert_eq!(effective(&restored, "search"), original_config);
    let records = session.history().await?;
    assert_eq!(
        json!(&records[..original_records.len()]),
        json!(original_records)
    );
    assert!(
        !records
            .iter()
            .any(|record| record.kind == "configuration_commit")
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record.kind == "composition_lock")
            .count(),
        1
    );
    assert_eq!(latest_binding(&records), &binding);
    session.shutdown().await?;
    let reopened = Session::open_with(path, options).await?;
    assert_eq!(
        effective(&reopened.inspect_configuration().await?, "search"),
        original_config
    );
    assert_eq!(latest_binding(&reopened.history().await?), &binding);
    assert_eq!(
        reopened.configuration_operation(receipt.operation)?.status,
        Status::Restored
    );
    reopened.shutdown().await?;
    Ok(json!({
        "native_store_rejected_configuration_commit": true,
        "target_restored_without_restarting_store_or_model": true,
        "no_partial_configuration_commit_or_new_binding": true,
        "old_composition_and_failed_receipt_reopen": true,
    }))
}

#[tokio::main]
async fn main() -> ProbeResult<()> {
    let args: Vec<_> = std::env::args().collect();
    let composition = PathBuf::from(args.get(1).ok_or("default composition required")?);
    let failing = PathBuf::from(args.get(2).ok_or("failing-store composition required")?);
    let scratch = PathBuf::from(args.get(3).ok_or("isolated scratch directory required")?);
    std::fs::create_dir_all(&scratch)?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(40), async {
        Ok::<_, Box<dyn std::error::Error>>(json!({
            "committed": committed_configuration(&composition, &scratch).await?,
            "rejected": rejected_commit(&failing, &scratch).await?,
        }))
    })
    .await??;
    println!("{result}");
    Ok(())
}
