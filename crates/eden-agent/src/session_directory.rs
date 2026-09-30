//! Saved-session management never acquires or redirects a live Session implicitly.
use crate::{Fault, Session};
use eden_protocol::coding::Record;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize)]
#[allow(missing_docs)]
/// A damaged file remains discoverable with its validated prefix and diagnostic.
pub struct SavedSession {
    pub path: PathBuf,
    pub session_id: Option<u64>,
    pub cwd: Option<String>,
    pub name: String,
    pub tags: Vec<String>,
    pub modified: Option<u64>,
    pub diagnostic: Option<String>,
    pub records: usize,
}
fn fault(error: impl std::fmt::Display) -> Fault {
    Fault::new("SessionDirectory", "session-directory", error.to_string())
}
fn describe_records(
    path: PathBuf,
    records: Vec<Record>,
    diagnostic: Option<String>,
) -> SavedSession {
    let modified = path
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let metadata = records.iter().rev().find(|r| r.kind == "session_metadata");
    SavedSession {
        session_id: records.first().map(|r| r.session_id),
        cwd: records
            .iter()
            .find(|r| r.kind == "session")
            .and_then(|r| r.payload["cwd"].as_str())
            .map(str::to_owned),
        name: metadata
            .and_then(|r| r.payload["name"].as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }),
        tags: metadata
            .and_then(|r| serde_json::from_value(r.payload["tags"].clone()).ok())
            .unwrap_or_default(),
        path,
        modified,
        diagnostic,
        records: records.len(),
    }
}
impl Session {
    /// Show effective project trust without exposing workspace settings or credential values.
    pub fn project_trust(&self) -> Result<serde_json::Value, Fault> {
        let workspace = crate::Workspace::discover(
            std::path::Path::new(self.cwd()),
            &self.0.workspace_options,
        )?;
        Ok(serde_json::json!({
            "cwd": workspace.cwd,
            "trusted": workspace.trusted,
            "startup_override": self.0.workspace_options.project_trust,
            "diagnostics": workspace.diagnostics,
        }))
    }
    /// Save an explicit trust choice. Current startup overrides retain precedence.
    pub fn save_project_trust(&self, path: &std::path::Path, trusted: bool) -> Result<(), Fault> {
        crate::save_trust(&self.0.workspace_options.global_dir, path, trusted)
    }

    /// Inspect one selected identity without opening a writer or executing its composition.
    pub async fn describe_saved_session(path: PathBuf) -> Result<SavedSession, Fault> {
        tokio::task::spawn_blocking(move || {
            describe_cancellable(path, &eden_plugin_sdk::Cancellation::default())?
                .ok_or_else(|| fault("inspection cancelled"))
        })
        .await
        .map_err(fault)?
    }

    /// Scan only the explicitly selected directory. Bad histories stay visible; no plugin runs.
    pub async fn list_saved_sessions(directory: PathBuf) -> Result<Vec<SavedSession>, Fault> {
        let mut scan = Self::scan_saved_sessions(directory);
        let mut items = vec![];
        while let Some(entry) = scan.next().await {
            items.push(entry?);
        }
        items.sort_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(items)
    }

    /// Read a complete tree without loading plugins or changing its active branch.
    pub async fn saved_session_tree(path: PathBuf) -> Result<Vec<Record>, Fault> {
        tokio::task::spawn_blocking(move || eden_kernel::history::read(&path))
            .await
            .map_err(fault)?
    }
    /// Delete an explicitly reviewed identity only while its ordinary writer lock is available.
    /// The sidecar remains in place so a concurrent opener cannot acquire a different lock inode.
    pub async fn delete_saved_session(path: PathBuf, expected_session: u64) -> Result<(), Fault> {
        tokio::task::spawn_blocking(move || {
            let path = std::fs::canonicalize(path).map_err(fault)?;
            let mut lock_path = path.as_os_str().to_owned();
            lock_path.push(".lock");
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(lock_path)
                .map_err(fault)?;
            lock.try_lock()
                .map_err(|_| fault("session is active or its writer lock is unavailable"))?;
            let scan = eden_kernel::history::inspect(&path)?;
            if scan.records.first().map(|r| r.session_id) != Some(expected_session) {
                return Err(fault("session identity changed; inspect it again"));
            }
            std::fs::remove_file(path).map_err(fault)
        })
        .await
        .map_err(fault)?
    }
}
/// Owns an incremental directory read. Dropping or cancelling it releases the producer.
pub struct SavedSessionScan {
    receiver: tokio::sync::mpsc::Receiver<Result<SavedSession, Fault>>,
    cancel: eden_plugin_sdk::Cancellation,
    worker: Option<tokio::task::JoinHandle<()>>,
}
impl SavedSessionScan {
    /// Wait for the next file; completion and cancellation both close the stream.
    pub async fn next(&mut self) -> Option<Result<SavedSession, Fault>> {
        self.receiver.recv().await
    }
    /// Cancel pending reads and discard queued rows; no later row is delivered.
    pub fn cancel(&mut self) {
        self.cancel.cancel();
        self.receiver.close();
        while self.receiver.try_recv().is_ok() {}
    }
    /// Cancel and observe producer completion before releasing an explicit scan operation.
    pub async fn cancel_and_wait(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.await;
        }
        // A producer may already hold a channel permit when close() races its send.
        while self.receiver.try_recv().is_ok() {}
    }
    /// Drain already produced rows without delaying the next UI frame.
    pub fn try_next(&mut self) -> Option<Result<SavedSession, Fault>> {
        self.receiver.try_recv().ok()
    }
    /// A closed empty channel cannot deliver more rows; use cancel_and_wait for cancellation cleanup.
    pub fn finished(&self) -> bool {
        self.receiver.is_closed() && self.receiver.is_empty()
    }
}
impl Drop for SavedSessionScan {
    fn drop(&mut self) {
        self.cancel();
    }
}
impl Session {
    /// Read entries incrementally off the async/UI thread, with bounded queued metadata.
    pub fn scan_saved_sessions(directory: PathBuf) -> SavedSessionScan {
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        let cancel = eden_plugin_sdk::Cancellation::default();
        let worker_cancel = cancel.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let result = (|| -> Result<(), Fault> {
                let directory = match std::fs::read_dir(directory) {
                    Ok(d) => d,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(e) => return Err(fault(e)),
                };
                for entry in directory {
                    if worker_cancel.is_cancelled() {
                        break;
                    }
                    let entry = entry.map_err(fault)?;
                    let path = entry.path();
                    if path.extension().is_none_or(|e| e != "jsonl")
                        || !entry.file_type().map_err(fault)?.is_file()
                    {
                        continue;
                    }
                    let item = match describe_cancellable(path.clone(), &worker_cancel) {
                        Ok(Some(entry)) => entry,
                        Ok(None) => break,
                        Err(error) => describe_records(path, vec![], Some(error.to_string())),
                    };
                    if sender.blocking_send(Ok(item)).is_err() {
                        break;
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                let _ = sender.blocking_send(Err(error));
            }
        });
        SavedSessionScan {
            receiver,
            cancel,
            worker: Some(worker),
        }
    }
}
fn describe_cancellable(
    path: PathBuf,
    cancel: &eden_plugin_sdk::Cancellation,
) -> Result<Option<SavedSession>, Fault> {
    use std::io::Read;
    let mut file = std::fs::File::open(&path).map_err(fault)?;
    let mut bytes = Vec::new();
    let mut chunk = [0; 65536];
    loop {
        if cancel.is_cancelled() {
            return Ok(None);
        }
        let n = file.read(&mut chunk).map_err(fault)?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    let scan = eden_protocol::history::scan_records(&bytes);
    if cancel.is_cancelled() {
        return Ok(None);
    }
    Ok(Some(describe_records(path, scan.records, scan.diagnostic)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn active_history_cannot_be_deleted_and_bad_history_stays_visible() {
        let directory = std::env::temp_dir().join(format!("eden-directory-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("session.jsonl");
        std::fs::write(&path, b"{broken").unwrap();
        let entries = Session::list_saved_sessions(directory.clone())
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].diagnostic.is_some());
        let lock = std::fs::File::create(directory.join("session.jsonl.lock")).unwrap();
        lock.try_lock().unwrap();
        assert!(
            Session::delete_saved_session(path.clone(), 1)
                .await
                .unwrap_err()
                .message
                .contains("active")
        );
        assert!(path.exists());
        drop(lock);
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[cfg(test)]
mod scan_tests {
    use super::*;
    #[tokio::test]
    async fn incremental_reader_delivers_before_completion_and_cancellation_joins_worker() {
        let directory = std::env::temp_dir().join(format!("eden-scan-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        for i in 0..40 {
            std::fs::write(directory.join(format!("{i}.jsonl")), b"{bad").unwrap();
        }
        let mut scan = Session::scan_saved_sessions(directory.clone());
        assert!(scan.next().await.unwrap().unwrap().diagnostic.is_some());
        scan.cancel_and_wait().await;
        assert!(scan.next().await.is_none());
        assert!(scan.worker.is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
