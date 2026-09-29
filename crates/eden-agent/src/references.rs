//! Read-only saved-session discovery and source projection, independent of source writers.
use crate::{Fault, InputOwner, Session};
use eden_protocol::{
    coding::{ContextInput, ModelInput, ModelLimits, Record},
    context_edit, history,
    session_reference::{self as r, Branch, CatalogEntry, Preview, Source},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
fn failure(error: impl std::fmt::Display) -> Fault {
    Fault::new("InvalidReference", "session-reference", error.to_string())
}
fn source(path: &Path, records: &[Record]) -> Result<Source, Fault> {
    let (head, branch) = history::branch_state(records)?;
    Ok(Source {
        session_id: records.first().map_or(0, |record| record.session_id),
        label: path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        path: path.to_string_lossy().into_owned(),
        branch,
        head,
    })
}
fn selected_records(path: &Path, head: Option<u64>) -> Result<Vec<Record>, Fault> {
    let mut records = eden_kernel::history::read(path)?;
    if let Some(head) = head {
        let record = records
            .iter()
            .find(|record| record.sequence == head)
            .ok_or_else(|| failure("source head is unavailable"))?;
        if record.kind == "branch_selected" {
            return Err(failure("source head must be a tree node"));
        }
        records.truncate(head as usize);
    }
    Ok(records)
}
fn catalog(directory: &Path) -> Result<Vec<CatalogEntry>, Fault> {
    let directory = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(failure(error)),
    };
    let mut entries = Vec::new();
    for entry in directory {
        let entry = entry.map_err(failure)?;
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "jsonl")
            || !entry.file_type().map_err(failure)?.is_file()
        {
            continue;
        }
        let scan = eden_kernel::history::inspect(&path)?;
        entries.push(CatalogEntry {
            source: source(&path, &scan.records)?,
            diagnostic: scan.diagnostic,
        });
    }
    entries.sort_by(|a, b| a.source.path.cmp(&b.source.path));
    Ok(entries)
}
impl Session {
    /// Discover local saved sessions without opening them, repairing tails or acquiring writers.
    pub async fn session_catalog(&self) -> Result<Vec<CatalogEntry>, Fault> {
        let directory = PathBuf::from(&self.0.cwd).join(".eden/sessions");
        tokio::task::spawn_blocking(move || catalog(&directory))
            .await
            .map_err(failure)?
    }
    /// Return each branch's latest durable head without changing either session's selection.
    pub async fn session_branches(&self, path: impl AsRef<Path>) -> Result<Vec<Branch>, Fault> {
        let path = self.reference_path(path.as_ref());
        tokio::task::spawn_blocking(move || {
            let records = eden_kernel::history::read(&path)?;
            let mut branches = BTreeMap::new();
            for record in records {
                if record.kind != "branch_selected" {
                    branches.insert(record.branch, record.sequence);
                }
            }
            Ok(branches
                .into_iter()
                .map(|(name, head)| Branch { name, head })
                .collect())
        })
        .await
        .map_err(failure)?
    }
    /// Capture the selected source's effective context on a background task. No source work runs.
    pub async fn reference_preview(
        &self,
        path: impl AsRef<Path>,
        head: Option<u64>,
    ) -> Result<Preview, Fault> {
        let path = self.reference_path(path.as_ref());
        let run_id = self.admit_input()?;
        let session = self.clone();
        let owner = InputOwner(session.clone());
        tokio::spawn(async move {
            let _owner = owner;
            let (source, input, source_system) = tokio::task::spawn_blocking(move || {
                let records = selected_records(&path, head)?;
                let source = source(&path, &records)?;
                let active = history::active_path(&records)?;
                let last_request: Option<ModelInput> = active
                    .iter()
                    .rev()
                    .filter(|r| {
                        matches!(r.kind.as_str(), "model_request" | "model_request_revision")
                    })
                    .find_map(|r| r.payload.get("input"))
                    .map(|input| serde_json::from_value(input.clone()).map_err(failure))
                    .transpose()?;
                let source_system = last_request
                    .as_ref()
                    .map(|request| r::system_items(&request.items));
                let resources = active
                    .iter()
                    .rev()
                    .find(|r| r.kind == "resource_snapshot")
                    .map(|r| serde_json::from_value(r.payload.clone()).map_err(failure))
                    .transpose()?;
                let cwd = active
                    .iter()
                    .find_map(|r| r.payload.get("cwd").and_then(serde_json::Value::as_str))
                    .unwrap_or_default()
                    .to_owned();
                let input = ContextInput {
                    target: last_request.as_ref().and_then(|r| r.target.clone()),
                    resources,
                    tools: Some(
                        last_request
                            .as_ref()
                            .map(|r| r.tools.clone())
                            .unwrap_or_default(),
                    ),
                    action: "inspect".into(),
                    records,
                    instructions: String::new(),
                    limits: ModelLimits::default(),
                    cwd,
                    items: vec![],
                };
                Ok::<_, Fault>((source, input, source_system))
            })
            .await
            .map_err(failure)??;
            let snapshot: context_edit::Snapshot = session
                .service_input(
                    run_id,
                    context_edit::SERVICE,
                    &context_edit::Request::ProjectSource { input },
                )
                .await?;
            let mut preview = Preview {
                source,
                entries: snapshot
                    .effective
                    .entries
                    .into_iter()
                    .filter(|e| r::selectable(&e.item))
                    .collect(),
                source_system,
                projection_version: 1,
                estimated_tokens: 0,
            };
            preview.estimated_tokens = r::estimate_blocks(
                &preview
                    .freeze("estimate".into(), &r::Selection::default())?
                    .content,
            );
            Ok(preview)
        })
        .await
        .map_err(failure)?
    }
    fn reference_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_owned()
        } else {
            Path::new(&self.0.cwd).join(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_source_branch_is_read_only_and_damage_is_never_hidden() {
        let directory = std::env::temp_dir().join(format!("eden-reference-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("source.jsonl");
        let records = vec![
            Record {
                schema_version: 2,
                session_id: 9,
                sequence: 1,
                run_id: 1,
                parent_id: None,
                branch: "main".into(),
                kind: "message".into(),
                payload: serde_json::json!({}),
            },
            Record {
                schema_version: 2,
                session_id: 9,
                sequence: 2,
                run_id: 1,
                parent_id: Some(1),
                branch: "main".into(),
                kind: "message".into(),
                payload: serde_json::json!({}),
            },
            Record {
                schema_version: 2,
                session_id: 9,
                sequence: 3,
                run_id: 1,
                parent_id: Some(1),
                branch: "other".into(),
                kind: "message".into(),
                payload: serde_json::json!({}),
            },
        ];
        let bytes = history::encode_transaction(&records).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let selected = selected_records(&path, Some(2)).unwrap();
        assert_eq!(
            history::active_path(&selected)
                .unwrap()
                .iter()
                .map(|r| r.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(source(&path, &selected).unwrap().branch, "main");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let mut damaged = bytes;
        damaged.extend_from_slice(b"{broken");
        std::fs::write(&path, &damaged).unwrap();
        assert!(catalog(&directory).unwrap()[0].diagnostic.is_some());
        assert!(selected_records(&path, Some(2)).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), damaged);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
