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
    pub name: String,
    pub tags: Vec<String>,
    pub modified: Option<u64>,
    pub diagnostic: Option<String>,
    pub records: usize,
}
fn fault(error: impl std::fmt::Display) -> Fault {
    Fault::new("SessionDirectory", "session-directory", error.to_string())
}
fn describe(path: PathBuf) -> SavedSession {
    let modified = path
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let (records, diagnostic) = match eden_kernel::history::inspect(&path) {
        Ok(scan) => (scan.records, scan.diagnostic),
        Err(error) => (vec![], Some(error.to_string())),
    };
    let metadata = records.iter().rev().find(|r| r.kind == "session_metadata");
    SavedSession {
        session_id: records.first().map(|r| r.session_id),
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

    /// Scan only the explicitly selected directory. Bad histories stay visible; no plugin runs.
    pub async fn list_saved_sessions(directory: PathBuf) -> Result<Vec<SavedSession>, Fault> {
        tokio::task::spawn_blocking(move || {
            let directory = match std::fs::read_dir(directory) {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
                Err(e) => return Err(fault(e)),
            };
            let mut items = vec![];
            for entry in directory {
                let entry = entry.map_err(fault)?;
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "jsonl")
                    && entry.file_type().map_err(fault)?.is_file()
                {
                    items.push(describe(path));
                }
            }
            items.sort_by(|a, b| {
                b.modified
                    .cmp(&a.modified)
                    .then_with(|| a.path.cmp(&b.path))
            });
            Ok(items)
        })
        .await
        .map_err(fault)?
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
