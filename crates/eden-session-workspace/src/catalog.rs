//! Reversible history mutations use the same writer arbitration as normal execution.
use crate::runtime::{Lifecycle, Opened, fault};
use eden_agent::{SavedSession, Session};
use eden_protocol::Fault;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// A reviewed history position; a path by itself is never authorization to remove data.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryRef {
    /// Canonical location is distinct from the identity retained inside a copied file.
    pub path: PathBuf,
    /// Persistent identity read from the validated header.
    pub session_id: Option<u64>,
    /// Detect changes while a stopped or idle history is being reviewed.
    pub bytes: u64,
    /// Filesystem revision, used together with identity and length.
    pub modified_ns: u128,
}

/// Immutable confirmation material retained by the consumer until apply or cancellation.
#[derive(Clone, Debug, Serialize)]
pub struct RemovalPlan {
    /// Exact history the user selected.
    pub history: HistoryRef,
    /// Human-readable target shown in confirmation.
    pub title: String,
    /// Live ownership is bound to this incarnation, not a later replacement.
    pub owner: Option<Opened>,
    /// Older hosts must be stopped explicitly before reviewing removal; they cannot seal admission.
    pub reviewed_stop: bool,
    /// Work which the explicit stop confirmation covers.
    pub active_run: Option<u64>,
    /// Independent shell work is included in stop confirmation.
    pub shell_runs: Vec<u64>,
    /// Extension commands also own work.
    pub command_runs: Vec<u64>,
}

/// A durable, reversible removal receipt. The original public records remain unchanged.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrashEntry {
    /// Opaque operation identity within this history directory.
    pub id: String,
    /// Saved history identity.
    pub session_id: Option<u64>,
    /// Restore destination. Restore never overwrites an occupied path.
    pub original: PathBuf,
    /// User-facing title retained even if a damaged body cannot be described.
    pub title: String,
    /// Project association for browsing the same catalog after restart.
    pub cwd: Option<String>,
    /// Removal time, in Unix seconds.
    pub removed_at: u64,
    /// Directory containing the manifest and retained public history.
    pub directory: PathBuf,
}

fn io(error: impl std::fmt::Display) -> Fault {
    fault("HistoryMutation", error.to_string())
}
fn inspect_ref(path: &Path) -> Result<HistoryRef, Fault> {
    let path = std::fs::canonicalize(path).map_err(io)?;
    let metadata = path.metadata().map_err(io)?;
    let scan = eden_protocol::history::scan_records(&std::fs::read(&path).map_err(io)?);
    let session_id = scan.records.first().map(|record| record.session_id);
    Ok(HistoryRef {
        path,
        session_id,
        bytes: metadata.len(),
        modified_ns: metadata
            .modified()
            .map_err(io)?
            .duration_since(UNIX_EPOCH)
            .map_err(io)?
            .as_nanos(),
    })
}
fn lock_writer(path: &Path) -> Result<File, Fault> {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock)
        .map_err(io)?;
    file.try_lock().map_err(|_| {
        fault(
            "WriterConflict",
            "another writer owns this history; refresh its status",
        )
    })?;
    Ok(file)
}
fn sync_directory(path: &Path) -> Result<(), Fault> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io)?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn title(info: &SavedSession) -> String {
    if info.has_name {
        info.name.clone()
    } else {
        info.summary
            .clone()
            .unwrap_or_else(|| "Untitled session".into())
    }
}

impl Lifecycle {
    /// Route metadata to a verified live writer, or mutate a stopped history under its lock.
    pub async fn rename_history(
        &self,
        path: &Path,
        name: String,
        tags: Option<Vec<String>>,
    ) -> Result<(), Fault> {
        if let Some(owner) = self.owner(path).await? {
            let client = eden_tui_client::HostClient::for_instance(
                &owner.endpoint,
                owner.session_id,
                &owner.instance,
            )?;
            let request_id = format!(
                "metadata-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(io)?
                    .as_nanos()
            );
            let reply = client
                .request(
                    "POST",
                    "/manage/metadata",
                    Some(&serde_json::json!({
                        "request_id": request_id,
                        "name": name,
                        "preserve_tags": tags.is_none(),
                        "tags": tags.unwrap_or_default(),
                    })),
                )
                .await?;
            let run = reply["run_id"]
                .as_u64()
                .ok_or_else(|| io("missing metadata operation"))?;
            client.wait(run).await?.into_result()?;
            Ok(())
        } else {
            Session::saved_metadata(path.to_owned(), name, tags).await
        }
    }
    /// Review a selected history without changing its owner or acquiring a new execution host.
    pub async fn plan_removal(&self, path: &Path) -> Result<RemovalPlan, Fault> {
        let path = path.to_owned();
        let history = tokio::task::spawn_blocking(move || inspect_ref(&path))
            .await
            .map_err(io)??;
        let info = Session::describe_saved_session(history.path.clone()).await?;
        let owner = self.owner(&history.path).await?;
        let state = if let Some(owner) = &owner {
            Some(
                eden_tui_client::HostClient::for_instance(
                    &owner.endpoint,
                    owner.session_id,
                    &owner.instance,
                )?
                .verify()
                .await?,
            )
        } else {
            None
        };
        Ok(RemovalPlan {
            history,
            title: title(&info),
            owner,
            reviewed_stop: state.as_ref().is_none_or(|state| state.reviewed_stop),
            active_run: state.as_ref().and_then(|s| s.active_run),
            shell_runs: state
                .as_ref()
                .map(|s| s.shell_runs.clone())
                .unwrap_or_default(),
            command_runs: state
                .as_ref()
                .map(|s| s.command_runs.clone())
                .unwrap_or_default(),
        })
    }

    /// Apply the reviewed removal after explicit authorization to close its live owner.
    /// A replacement owner, newly started work, or a competing writer invalidates the plan.
    pub async fn remove_history(
        &self,
        plan: RemovalPlan,
        stop_owner: bool,
    ) -> Result<TrashEntry, Fault> {
        let owner = self.owner(&plan.history.path).await?;
        if owner.as_ref().map(|o| &o.instance) != plan.owner.as_ref().map(|o| &o.instance) {
            return Err(fault(
                "StaleSelection",
                "session ownership changed; review the deletion again",
            ));
        }
        let running = plan.active_run.is_some()
            || !plan.shell_runs.is_empty()
            || !plan.command_runs.is_empty();
        let current = inspect_ref(&plan.history.path)?;
        if current.session_id != plan.history.session_id || (!running && current != plan.history) {
            return Err(fault(
                "StaleSelection",
                "history changed; review the deletion again",
            ));
        }
        if let Some(owner) = owner {
            if !plan.reviewed_stop {
                return Err(fault(
                    "StopRequired",
                    "stop this older host explicitly, then review moving its history to Trash",
                ));
            }
            if !stop_owner {
                return Err(fault(
                    "StopRequired",
                    "confirm closing this session before moving it to Trash",
                ));
            }
            let state = eden_tui_client::HostClient::for_instance(
                &owner.endpoint,
                owner.session_id,
                &owner.instance,
            )?
            .verify()
            .await?;
            if state.active_run != plan.active_run
                || state.shell_runs != plan.shell_runs
                || state.command_runs != plan.command_runs
                || state.managing
                || state.pending_inputs != 0
            {
                return Err(fault(
                    "StaleSelection",
                    "session work changed; review the deletion again",
                ));
            }
            self.close_reviewed(
                &owner,
                Some(eden_agent::StopExpectation {
                    active_run: plan.active_run,
                    shell_runs: plan.shell_runs.clone(),
                    command_runs: plan.command_runs.clone(),
                }),
            )
            .await?;
        }
        let expected = if running {
            inspect_ref(&plan.history.path)?
        } else {
            plan.history
        };
        tokio::task::spawn_blocking(move || move_to_trash(&expected, &plan.title))
            .await
            .map_err(io)?
    }

    /// Read completed removals from a selected history directory; ordinary listing stays separate.
    pub async fn list_trash(&self, directory: &Path) -> Result<Vec<TrashEntry>, Fault> {
        let directory = directory.to_owned();
        tokio::task::spawn_blocking(move || {
            let root = directory.join(".trash");
            let entries = match std::fs::read_dir(&root) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
                Err(e) => return Err(io(e)),
            };
            let mut result = vec![];
            for entry in entries {
                let directory = entry.map_err(io)?.path();
                if !directory.join("history.jsonl").is_file() {
                    continue;
                }
                let item = read_trash(&directory)?;
                result.push(item);
            }
            result.sort_by(|a, b| {
                b.removed_at
                    .cmp(&a.removed_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
            Ok(result)
        })
        .await
        .map_err(io)?
    }

    /// Restore full records to their original location. Collision or writer conflict retains Trash.
    pub async fn restore_history(&self, entry: &TrashEntry) -> Result<PathBuf, Fault> {
        let entry = entry.clone();
        tokio::task::spawn_blocking(move || {
            let current = read_trash(&entry.directory)?;
            if current.id != entry.id
                || current.session_id != entry.session_id
                || current.original != entry.original
            {
                return Err(fault(
                    "StaleSelection",
                    "Trash entry changed; refresh the catalog",
                ));
            }
            let _lock = lock_writer(&current.original)?;
            let saved = current.directory.join("history.jsonl");
            if inspect_ref(&saved)?.session_id != current.session_id {
                return Err(fault("IdentityMismatch", "Trash history identity changed"));
            }
            // Same-filesystem hard-link publication is atomic and never replaces a target.
            // Publication may have survived a crash before the Trash link was removed.
            // Equal complete records are already restored; never replace the destination.
            if let Err(error) = std::fs::hard_link(&saved, &current.original)
                && (error.kind() != std::io::ErrorKind::AlreadyExists
                    || std::fs::read(&saved).map_err(io)?
                        != std::fs::read(&current.original).map_err(io)?)
            {
                return Err(io(error));
            }
            sync_directory(
                current
                    .original
                    .parent()
                    .ok_or_else(|| io("missing original directory"))?,
            )?;
            std::fs::remove_file(&saved).map_err(io)?;
            sync_directory(&current.directory)?;
            Ok(current.original)
        })
        .await
        .map_err(io)?
    }
}

pub(crate) fn read_trash(directory: &Path) -> Result<TrashEntry, Fault> {
    let directory = std::fs::canonicalize(directory).map_err(io)?;
    let bytes = std::fs::read(directory.join("entry.json")).map_err(io)?;
    let mut item: TrashEntry = serde_json::from_slice(&bytes).map_err(io)?;
    let parent = directory
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io("invalid Trash location"))?;
    if directory.parent().and_then(Path::file_name) != Some(std::ffi::OsStr::new(".trash"))
        || item.original.parent() != Some(parent)
        || directory.file_name().and_then(|name| name.to_str()) != Some(item.id.as_str())
    {
        return Err(fault(
            "IdentityMismatch",
            "Trash manifest does not match its location",
        ));
    }
    item.directory = directory;
    Ok(item)
}

fn move_to_trash(expected: &HistoryRef, title: &str) -> Result<TrashEntry, Fault> {
    let _lock = lock_writer(&expected.path)?;
    let current = inspect_ref(&expected.path)?;
    if current != *expected {
        return Err(fault(
            "StaleSelection",
            "selected history changed; review the deletion again",
        ));
    }
    let parent = expected
        .path
        .parent()
        .ok_or_else(|| io("missing history directory"))?;
    let trash = parent.join(".trash");
    std::fs::create_dir_all(&trash).map_err(io)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(io)?;
    let id = format!("{}-{}", now.as_nanos(), std::process::id());
    let directory = trash.join(&id);
    std::fs::create_dir(&directory).map_err(io)?;
    let scan = eden_protocol::history::scan_records(&std::fs::read(&expected.path).map_err(io)?);
    let info = scan
        .records
        .iter()
        .find(|r| r.kind == "session")
        .and_then(|r| r.payload["cwd"].as_str())
        .map(str::to_owned);
    let item = TrashEntry {
        id,
        session_id: expected.session_id,
        original: expected.path.clone(),
        title: title.into(),
        cwd: info,
        removed_at: now.as_secs(),
        directory: directory.clone(),
    };
    let mut manifest = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(directory.join("entry.json"))
        .map_err(io)?;
    manifest
        .write_all(&serde_json::to_vec(&item).map_err(io)?)
        .and_then(|_| manifest.sync_all())
        .map_err(io)?;
    sync_directory(&directory)?;
    sync_directory(&trash)?;
    std::fs::rename(&expected.path, directory.join("history.jsonl")).map_err(io)?;
    sync_directory(&directory)?;
    sync_directory(parent)?;
    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(name: &str) -> (Lifecycle, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "eden-trash-{}-{}-{name}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let runtime = Lifecycle::new(
            root.join("unused"),
            vec![],
            root.clone(),
            root.join("state"),
            vec![],
        );
        let path = root.join("test.jsonl");
        let record = eden_protocol::coding::Record {
            schema_version: 2,
            session_id: 17,
            sequence: 1,
            run_id: 0,
            parent_id: None,
            branch: "main".into(),
            kind: "session".into(),
            payload: serde_json::json!({ "cwd": root }),
        };
        std::fs::write(
            &path,
            eden_protocol::history::encode_transaction(&[record]).unwrap(),
        )
        .unwrap();
        (runtime, path)
    }
    #[tokio::test]
    async fn removal_survives_restart_and_restore_keeps_exact_history() {
        let (runtime, path) = fixture("restore");
        let original = std::fs::read(&path).unwrap();
        let plan = runtime.plan_removal(&path).await.unwrap();
        let item = runtime.remove_history(plan, false).await.unwrap();
        assert!(!path.exists());
        assert_eq!(
            std::fs::read(item.directory.join("history.jsonl")).unwrap(),
            original
        );
        let reopened = Lifecycle::new(
            runtime.executable.clone(),
            vec![],
            runtime.cwd.clone(),
            runtime.state_dir.clone(),
            vec![],
        );
        let entries = reopened.list_trash(path.parent().unwrap()).await.unwrap();
        assert_eq!(entries.len(), 1);
        reopened.restore_history(&entries[0]).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(
            reopened
                .list_trash(path.parent().unwrap())
                .await
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(runtime.cwd).unwrap();
    }
    #[tokio::test]
    async fn writer_and_restore_collision_keep_every_history() {
        let (runtime, path) = fixture("conflict");
        let original = std::fs::read(&path).unwrap();
        let plan = runtime.plan_removal(&path).await.unwrap();
        let lock = lock_writer(&path).unwrap();
        assert_eq!(
            runtime
                .remove_history(plan.clone(), false)
                .await
                .unwrap_err()
                .code,
            "WriterConflict"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        drop(lock);
        let item = runtime.remove_history(plan, false).await.unwrap();
        std::fs::write(&path, b"new destination").unwrap();
        assert!(runtime.restore_history(&item).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"new destination");
        assert_eq!(
            std::fs::read(item.directory.join("history.jsonl")).unwrap(),
            original
        );
        std::fs::remove_dir_all(runtime.cwd).unwrap();
    }
    #[tokio::test]
    async fn revision_is_rechecked_under_the_writer_lock() {
        let (runtime, path) = fixture("revision");
        let plan = runtime.plan_removal(&path).await.unwrap();
        Session::saved_metadata(path.clone(), "new name".into(), None)
            .await
            .unwrap();
        let changed = std::fs::read(&path).unwrap();
        assert_eq!(
            move_to_trash(&plan.history, &plan.title).unwrap_err().code,
            "StaleSelection"
        );
        assert_eq!(std::fs::read(&path).unwrap(), changed);
        std::fs::remove_dir_all(runtime.cwd).unwrap();
    }
    #[tokio::test]
    async fn restore_recovers_a_published_destination_after_interruption() {
        let (runtime, path) = fixture("restore-interrupted");
        let plan = runtime.plan_removal(&path).await.unwrap();
        let item = runtime.remove_history(plan, false).await.unwrap();
        let retained = item.directory.join("history.jsonl");
        let expected = std::fs::read(&retained).unwrap();
        std::fs::hard_link(&retained, &path).unwrap();
        runtime.restore_history(&item).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        assert!(!retained.exists());
        std::fs::remove_dir_all(runtime.cwd).unwrap();
    }
    #[tokio::test]
    async fn damaged_history_can_be_trashed_and_restored_without_parsing_or_repair() {
        let (runtime, path) = fixture("damaged");
        std::fs::write(&path, b"{damaged header\nuntouched tail").unwrap();
        let plan = runtime.plan_removal(&path).await.unwrap();
        assert_eq!(plan.history.session_id, None);
        let item = runtime.remove_history(plan, false).await.unwrap();
        runtime.restore_history(&item).await.unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{damaged header\nuntouched tail"
        );
        std::fs::remove_dir_all(runtime.cwd).unwrap();
    }
    #[tokio::test]
    async fn cold_rename_preserves_tags_activity_and_public_tree() {
        let (runtime, path) = fixture("rename");
        runtime
            .rename_history(&path, "第一项任务".into(), Some(vec!["work".into()]))
            .await
            .unwrap();
        let before = Session::describe_saved_session(path.clone()).await.unwrap();
        runtime
            .rename_history(&path, "Renamed".into(), None)
            .await
            .unwrap();
        let after = Session::describe_saved_session(path.clone()).await.unwrap();
        assert_eq!(after.name, "Renamed");
        assert_eq!(after.tags, before.tags);
        assert_eq!(after.activity, before.activity);
        assert_eq!(after.records, before.records + 1);
        assert!(
            eden_protocol::history::scan_records(&std::fs::read(&path).unwrap())
                .diagnostic
                .is_none()
        );
        assert!(!runtime.state_dir.exists());
        std::fs::remove_dir_all(runtime.cwd).unwrap();
    }
}
