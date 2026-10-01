//! Serial local public history, with an exclusive OS file lock and sync receipts.
use eden_plugin_sdk::{
    Package,
    protocol::{
        Descriptor, Fault,
        coding::*,
        history::{branch_state, encode_transaction, validate_records},
    },
    serde_json::Value,
};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, Write},
    sync::{Arc, Mutex},
};
#[derive(Default)]
struct Store {
    opened: bool,
    file: Option<File>,
    writer_lock: Option<File>,
    draft_path: Option<std::path::PathBuf>,
    records: Vec<Record>,
    id: u64,
    failed: bool,
}
impl Store {
    fn handle(&mut self, request: StoreRequest) -> Result<StoreReply, Fault> {
        match request {
            StoreRequest::OpenDraft {
                path,
                session_id,
                records,
            } => {
                if self.opened {
                    return Err(fault("already open"));
                }
                validate_records(&records)?;
                if records
                    .iter()
                    .any(|record| record.schema_version != 2 || record.session_id != session_id)
                {
                    return Err(fault("draft history must retain its v2 identity"));
                }
                let path = std::path::Path::new(&path);
                let parent = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(std::path::Path::new("."));
                let path = std::fs::canonicalize(parent).map_err(io)?.join(
                    path.file_name()
                        .ok_or_else(|| fault("history destination needs a filename"))?,
                );
                let writer_lock = acquire_lock(&path)?;
                if path.try_exists().map_err(io)? {
                    return Err(fault("draft destination already exists"));
                }
                self.writer_lock = Some(writer_lock);
                self.draft_path = Some(path);
                self.records = records;
                self.id = session_id;
                self.failed = false;
                self.opened = true;
            }
            StoreRequest::AdmitBatch { run_id, entries } => {
                self.available()?;
                let pending = self.prepare(run_id, entries, None)?;
                if let Some(path) = &self.draft_path {
                    let mut next = self.records.clone();
                    next.extend(pending);
                    validate_records(&next)?;
                    let file = match publish_open(path, &encode_transaction(&next)?) {
                        Ok(file) => file,
                        Err(error) => {
                            if error.code == "PublicationUncertain" {
                                self.failed = true;
                            }
                            return Err(error);
                        }
                    };
                    self.file = Some(file);
                    self.records = next;
                    self.draft_path = None;
                } else {
                    self.commit(pending)?;
                }
            }
            StoreRequest::RestoreMemory {
                session_id,
                records,
            } => {
                if self.opened {
                    return Err(fault("already open"));
                }
                validate_records(&records)?;
                if records
                    .iter()
                    .any(|record| record.schema_version != 2 || record.session_id != session_id)
                {
                    return Err(fault("restored memory history must retain its v2 identity"));
                }
                self.records = records;
                self.id = session_id;
                self.opened = true;
                self.failed = false;
            }
            StoreRequest::Open { path, session_id } => {
                if self.opened {
                    return Err(fault("already open"));
                }
                let (file, writer_lock, records) = if let Some(path) = path {
                    let path = std::path::Path::new(&path);
                    let canonical = match std::fs::canonicalize(path) {
                        Ok(path) => path,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            let parent = path
                                .parent()
                                .filter(|path| !path.as_os_str().is_empty())
                                .unwrap_or_else(|| std::path::Path::new("."));
                            std::fs::canonicalize(parent).map_err(io)?.join(
                                path.file_name()
                                    .ok_or_else(|| fault("history path needs a filename"))?,
                            )
                        }
                        Err(error) => return Err(io(error)),
                    };
                    let writer_lock = acquire_lock(&canonical)?;
                    let mut file = OpenOptions::new()
                        .create(true)
                        .truncate(false)
                        .read(true)
                        .write(true)
                        .open(&canonical)
                        .map_err(io)?;
                    let mut bytes = vec![];
                    file.read_to_end(&mut bytes).map_err(io)?;
                    let records = decode_records(&bytes)?;
                    if records
                        .first()
                        .is_some_and(|record| record.schema_version == 1)
                    {
                        return Err(fault(
                            "legacy history is read-only; explicit migration to a new v2 file is \
                             required",
                        ));
                    }
                    if records
                        .first()
                        .is_some_and(|record| record.session_id != session_id)
                    {
                        return Err(fault("session identity mismatch"));
                    }
                    (Some(file), Some(writer_lock), records)
                } else {
                    (None, None, vec![])
                };
                self.file = file;
                self.writer_lock = writer_lock;
                self.records = records;
                self.id = session_id;
                self.failed = false;
                self.opened = true;
            }
            StoreRequest::Create {
                path,
                session_id,
                records,
            } => {
                if self.opened {
                    return Err(fault("already open"));
                }
                validate_records(&records)?;
                if records
                    .iter()
                    .any(|record| record.schema_version != 2 || record.session_id != session_id)
                {
                    return Err(fault(
                        "new history requires v2 records with the requested identity",
                    ));
                }
                let bytes = encode_transaction(&records)?;
                let path = std::path::Path::new(&path);
                let parent = path
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or_else(|| std::path::Path::new("."));
                let parent = std::fs::canonicalize(parent).map_err(io)?;
                let path = parent.join(
                    path.file_name()
                        .ok_or_else(|| fault("history destination needs a filename"))?,
                );
                let writer_lock = acquire_lock(&path)?;
                if std::fs::symlink_metadata(&path).is_ok() {
                    return Err(fault("history destination already exists"));
                }
                publish_new(&path, &bytes)?;
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .map_err(io)?;
                self.file = Some(file);
                self.writer_lock = Some(writer_lock);
                self.records = records;
                self.id = session_id;
                self.failed = false;
                self.opened = true;
            }
            StoreRequest::Append {
                run_id,
                kind,
                payload,
            } => {
                self.append(run_id, vec![RecordDraft { kind, payload }])?;
            }
            StoreRequest::AppendBatch { run_id, entries } => self.append(run_id, entries)?,
            StoreRequest::AppendChecked {
                run_id,
                session_id,
                sequence,
                head,
                branch,
                new_branch,
                entries,
            } => {
                let (current_head, current_branch) = branch_state(&self.records)?;
                if self.id != session_id
                    || self.records.len() as u64 != sequence
                    || current_head != head
                    || current_branch != branch
                {
                    return Err(Fault::new(
                        "CheckpointConflict",
                        "local-history",
                        "history changed before checkpoint commit",
                    ));
                }
                if let Some(branch) = &new_branch
                    && (branch.trim().is_empty()
                        || self.records.iter().any(|record| &record.branch == branch))
                {
                    return Err(fault("new branch must be nonempty and unused"));
                }
                self.append_on(run_id, entries, new_branch)?;
            }
            StoreRequest::Navigate { target, branch } => {
                self.available()?;
                let record = Record {
                    schema_version: 2,
                    session_id: self.id,
                    sequence: self.records.len() as u64 + 1,
                    run_id: 0,
                    parent_id: Some(target),
                    branch: branch.clone(),
                    kind: "branch_selected".into(),
                    payload: serde_json::json!({ "target": target, "branch": branch }),
                };
                self.commit(vec![record])?;
            }
            StoreRequest::Read => {
                if !self.opened {
                    return Err(fault("store not open"));
                }
            }
            StoreRequest::Close => {
                self.file.take();
                self.writer_lock.take();
                self.draft_path.take();
                self.opened = false;
            }
        }
        let (active_head, active_branch) = branch_state(&self.records)?;
        Ok(StoreReply {
            session_id: self.id,
            sequence: self.records.len() as u64,
            records: self.records.clone(),
            active_head,
            active_branch,
        })
    }

    fn available(&self) -> Result<(), Fault> {
        if !self.opened || self.failed {
            Err(fault("store unavailable"))
        } else {
            Ok(())
        }
    }

    fn append(&mut self, run_id: u64, entries: Vec<RecordDraft>) -> Result<(), Fault> {
        self.append_on(run_id, entries, None)
    }
    fn append_on(
        &mut self,
        run_id: u64,
        entries: Vec<RecordDraft>,
        new_branch: Option<String>,
    ) -> Result<(), Fault> {
        let pending = self.prepare(run_id, entries, new_branch)?;
        self.commit(pending)
    }

    fn prepare(
        &self,
        run_id: u64,
        entries: Vec<RecordDraft>,
        new_branch: Option<String>,
    ) -> Result<Vec<Record>, Fault> {
        self.available()?;
        let (mut parent_id, branch) = branch_state(&self.records)?;
        let branch = new_branch.unwrap_or(branch);
        let mut pending = Vec::with_capacity(entries.len());
        for (index, entry) in entries.into_iter().enumerate() {
            if entry.kind == "branch_selected" {
                return Err(fault("branch selection requires Navigate"));
            }
            let sequence = self.records.len() as u64 + index as u64 + 1;
            pending.push(Record {
                schema_version: 2,
                session_id: self.id,
                sequence,
                run_id,
                parent_id,
                branch: branch.clone(),
                kind: entry.kind,
                payload: entry.payload,
            });
            parent_id = Some(sequence);
        }
        Ok(pending)
    }

    fn commit(&mut self, pending: Vec<Record>) -> Result<(), Fault> {
        let mut next = self.records.clone();
        next.extend(pending.iter().cloned());
        validate_records(&next)?;
        let bytes = encode_transaction(&pending)?;
        if let Some(file) = &mut self.file {
            // A failed write leaves an uncertain tail. Only explicit recovery may
            // create a new writable history; later appends cannot certify that tail.
            if let Err(error) = file
                .seek(std::io::SeekFrom::End(0))
                .and_then(|_| file.write_all(&bytes))
                .and_then(|_| file.sync_all())
            {
                self.failed = true;
                return Err(io(error));
            }
        }
        self.records = next;
        Ok(())
    }
}

fn acquire_lock(path: &std::path::Path) -> Result<File, Fault> {
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    // Keep writer arbitration separate from public data: Windows locks prohibit
    // readers too. Never unlink the sidecar while another process may own it.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(io)?;
    lock.try_lock().map_err(|error| {
        Fault::new(
            "WriterConflict",
            "local-history",
            format!("history already owned or lock unavailable: {error}"),
        )
    })?;
    Ok(lock)
}

/// A synced private staging file whose final public name is still absent.
///
/// Drop removes the staging file on write and publication failures, including
/// an unwinding panic, so a failed create cannot leave private temporary
/// history beside the public file.
struct PreparedHistory {
    file: Option<File>,
    path: std::path::PathBuf,
    cleanup: bool,
}
impl PreparedHistory {
    fn write(destination: &std::path::Path, bytes: &[u8]) -> Result<Self, Fault> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
        let parent = destination
            .parent()
            .ok_or_else(|| fault("destination parent unavailable"))?;
        let (prepared, mut file) = loop {
            let path = parent.join(format!(
                ".eden-history-{}-{}.tmp",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            match OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&path)
            {
                Ok(file) => {
                    break (
                        Self {
                            file: None,
                            path,
                            cleanup: true,
                        },
                        file,
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(io(error)),
            }
        };
        let write = file.write_all(bytes).and_then(|_| file.sync_all());
        write.map_err(io)?;
        let mut prepared = prepared;
        prepared.file = Some(file);
        Ok(prepared)
    }
    fn publish(mut self, destination: &std::path::Path) -> Result<File, Fault> {
        // A hard link exposes the complete synced bytes atomically and refuses
        // an existing destination, which create must never replace.
        std::fs::hard_link(&self.path, destination).map_err(io)?;
        if std::fs::remove_file(&self.path).is_ok() {
            self.cleanup = false;
        }
        Ok(self
            .file
            .take()
            .expect("prepared history owns its synced file"))
    }
}
impl Drop for PreparedHistory {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn publish_new(path: &std::path::Path, bytes: &[u8]) -> Result<(), Fault> {
    publish_open(path, bytes).map(|_| ())
}
fn publish_open(path: &std::path::Path, bytes: &[u8]) -> Result<File, Fault> {
    let file = PreparedHistory::write(path, bytes)?.publish(path)?;
    // Persist the new directory entry where directory sync is available.
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .ok_or_else(|| fault("destination parent unavailable"))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                Fault::new(
                    "PublicationUncertain",
                    "local-history",
                    format!(
                        "history was published but directory sync failed; reopen it before \
                         continuing: {error}"
                    ),
                )
            })?;
    }
    Ok(file)
}

fn fault(message: impl Into<String>) -> Fault {
    Fault::new("PersistenceFailure", "local-history", message)
}
fn io(error: std::io::Error) -> Fault {
    fault(error.to_string())
}
fn descriptor() -> Descriptor {
    Descriptor {
        package: "local-history".into(),
        version: "0.1.0".into(),
        provides: vec![STORE.into()],
    }
}
fn create(_: Value) -> Result<Package, Fault> {
    let store = Arc::new(Mutex::new(Store::default()));
    Ok(
        Package::new("local-history").service(STORE, move |request: StoreRequest, _| {
            let result = store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .handle(request);
            async move { result }
        }),
    )
}
eden_plugin_sdk::export_plugin!(descriptor, create);
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn draft_admission_publishes_initial_state_and_work_as_one_history() {
        let path =
            std::env::temp_dir().join(format!("eden-draft-admit-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut store = Store::default();
        store
            .handle(
                serde_json::from_value(json!({
                    "operation": "open_draft",
                    "path": path,
                    "session_id": 71,
                }))
                .unwrap(),
            )
            .unwrap();
        for kind in ["session", "composition_lock", "model_selection"] {
            store
                .handle(StoreRequest::Append {
                    run_id: 0,
                    kind: kind.into(),
                    payload: json!({}),
                })
                .unwrap();
        }
        assert!(
            !path.exists(),
            "configuration alone must not create saved history"
        );
        assert_eq!(
            Store::default()
                .handle(StoreRequest::Open {
                    path: Some(path.to_string_lossy().into()),
                    session_id: 71
                })
                .unwrap_err()
                .code,
            "WriterConflict"
        );
        let reply = store
            .handle(
                serde_json::from_value(json!({
                    "operation": "admit_batch",
                    "run_id": 4,
                    "entries": [{
                        "kind": "work_admitted",
                        "payload": { "command": "printf kept" },
                    }],
                }))
                .unwrap(),
            )
            .unwrap();
        let durable = decode_records(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(&durable).unwrap(),
            serde_json::to_value(&reply.records).unwrap()
        );
        assert_eq!(durable.last().unwrap().kind, "work_admitted");
        store.handle(StoreRequest::Close).unwrap();
        let mut reopened = Store::default();
        assert_eq!(
            reopened
                .handle(StoreRequest::Open {
                    path: Some(path.to_string_lossy().into()),
                    session_id: 71
                })
                .unwrap()
                .sequence,
            4
        );
        reopened.handle(StoreRequest::Close).unwrap();
        clean(&path);
    }

    #[test]
    fn draft_close_discards_staged_state_and_admission_never_overwrites() {
        let path =
            std::env::temp_dir().join(format!("eden-draft-discard-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let open = || {
            serde_json::from_value(json!({
                "operation": "open_draft",
                "path": path,
                "session_id": 72,
            }))
            .unwrap()
        };
        let mut store = Store::default();
        store.handle(open()).unwrap();
        store
            .handle(StoreRequest::Append {
                run_id: 0,
                kind: "session".into(),
                payload: json!({}),
            })
            .unwrap();
        store.handle(StoreRequest::Close).unwrap();
        assert!(!path.exists());
        store.handle(open()).unwrap();
        std::fs::write(&path, b"external destination").unwrap();
        let admit = || {
            serde_json::from_value(json!({
                "operation": "admit_batch",
                "run_id": 1,
                "entries": [{ "kind": "work_admitted", "payload": {} }],
            }))
            .unwrap()
        };
        assert!(store.handle(admit()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"external destination");
        assert_eq!(store.handle(StoreRequest::Read).unwrap().sequence, 0);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(store.handle(admit()).unwrap().sequence, 1);
        store.handle(StoreRequest::Close).unwrap();
        clean(&path);
    }
    #[test]
    fn checkpoint_rejects_stale_head_without_partial_notes() {
        let mut store = Store::default();
        store
            .handle(StoreRequest::Open {
                path: None,
                session_id: 7,
            })
            .unwrap();
        let request = StoreRequest::AppendChecked {
            new_branch: None,
            run_id: 1,
            session_id: 7,
            sequence: 0,
            head: None,
            branch: "main".into(),
            entries: vec![
                RecordDraft {
                    kind: "extension_state".into(),
                    payload: serde_json::json!({ "summary": "notes" }),
                },
                RecordDraft {
                    kind: "compaction".into(),
                    payload: serde_json::json!({ "summary": "notes", "first_kept": 0 }),
                },
            ],
        };
        store.handle(request.clone()).unwrap();
        assert_eq!(
            store.handle(request).unwrap_err().code,
            "CheckpointConflict"
        );
        assert_eq!(store.records.len(), 2);
    }

    #[test]
    fn memory_receipt_is_ordered_and_creates_no_file() {
        let mut store = Store::default();
        store
            .handle(StoreRequest::Open {
                path: None,
                session_id: 42,
            })
            .unwrap();
        let receipt = store
            .handle(StoreRequest::Append {
                run_id: 1,
                kind: "intent".into(),
                payload: json!({ "path": "a" }),
            })
            .unwrap();
        assert_eq!(receipt.sequence, 1);
        assert_eq!(receipt.records[0].session_id, 42);
        assert!(store.file.is_none());
    }
    #[test]
    fn durable_receipt_reopens_and_excludes_second_writer() {
        let path = std::env::temp_dir().join(format!("eden-store-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let open = || StoreRequest::Open {
            path: Some(path.to_string_lossy().into()),
            session_id: 43,
        };
        let mut first = Store::default();
        first.handle(open()).unwrap();
        let mut second = Store::default();
        assert_eq!(second.handle(open()).unwrap_err().code, "WriterConflict");
        let receipt = first
            .handle(StoreRequest::Append {
                run_id: 1,
                kind: "tool_intent".into(),
                payload: json!({ "name": "write" }),
            })
            .unwrap();
        assert_eq!(
            decode_records(&std::fs::read(&path).unwrap()).unwrap()[0].kind,
            "tool_intent"
        );
        first.handle(StoreRequest::Close).unwrap();
        assert_eq!(second.handle(open()).unwrap().sequence, receipt.sequence);
        second.handle(StoreRequest::Close).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"partial\":")
            .unwrap();
        assert_eq!(
            Store::default().handle(open()).unwrap_err().code,
            "PersistenceFailure"
        );
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(lock_path).unwrap();
    }
    #[test]
    fn legacy_writer_requires_explicit_copy_and_preserves_source() {
        let path =
            std::env::temp_dir().join(format!("eden-store-legacy-{}.jsonl", std::process::id()));
        let text = "{\"schema_version\":1,\"session_id\":7,\"sequence\":1,\"run_id\":1,\"kind\":\"\
                    user\",\"payload\":{}}\n";
        let bytes = text.as_bytes();
        std::fs::write(&path, bytes).unwrap();
        let result = Store::default().handle(StoreRequest::Open {
            path: Some(path.to_string_lossy().into()),
            session_id: 7,
        });
        assert!(result.unwrap_err().message.contains("migration"));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        clean(&path);
    }
    #[test]
    fn branch_navigation_and_atomic_batch_survive_reopen() {
        let path =
            std::env::temp_dir().join(format!("eden-store-branch-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let open = || StoreRequest::Open {
            path: Some(path.to_string_lossy().into()),
            session_id: 8,
        };
        let mut store = Store::default();
        store.handle(open()).unwrap();
        store
            .handle(StoreRequest::AppendBatch {
                run_id: 1,
                entries: vec![
                    RecordDraft {
                        kind: "user".into(),
                        payload: json!({}),
                    },
                    RecordDraft {
                        kind: "assistant".into(),
                        payload: json!({}),
                    },
                ],
            })
            .unwrap();
        store
            .handle(StoreRequest::Navigate {
                target: 1,
                branch: "alternate".into(),
            })
            .unwrap();
        store.handle(StoreRequest::Close).unwrap();
        let receipt = store.handle(open()).unwrap();
        assert_eq!(receipt.active_head, Some(1));
        assert_eq!(receipt.active_branch, "alternate");
        let receipt = store
            .handle(StoreRequest::Append {
                run_id: 2,
                kind: "user".into(),
                payload: json!({}),
            })
            .unwrap();
        assert_eq!(receipt.records[3].parent_id, Some(1));
        assert_eq!(receipt.records[1].sequence, 2);
        store.handle(StoreRequest::Close).unwrap();
        let records = decode_records(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(records.len(), 4);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 3);
        clean(&path);
    }
    #[test]
    fn create_is_exclusive_and_memory_validation_failure_is_atomic() {
        let path =
            std::env::temp_dir().join(format!("eden-store-copy-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut memory = Store::default();
        memory
            .handle(StoreRequest::Open {
                path: None,
                session_id: 9,
            })
            .unwrap();
        memory
            .handle(StoreRequest::Append {
                run_id: 1,
                kind: "user".into(),
                payload: json!({}),
            })
            .unwrap();
        assert!(
            memory
                .handle(StoreRequest::AppendBatch {
                    run_id: 1,
                    entries: vec![
                        RecordDraft {
                            kind: "assistant".into(),
                            payload: json!({})
                        },
                        RecordDraft {
                            kind: "branch_selected".into(),
                            payload: json!({ "target": 999 })
                        }
                    ]
                })
                .is_err()
        );
        assert_eq!(memory.handle(StoreRequest::Read).unwrap().sequence, 1);
        assert!(memory.file.is_none());
        assert!(memory.writer_lock.is_none());
        let request = || StoreRequest::Create {
            path: path.to_string_lossy().into(),
            session_id: 9,
            records: memory.records.clone(),
        };
        let mut copy = Store::default();
        copy.handle(request()).unwrap();
        copy.handle(StoreRequest::Close).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(Store::default().handle(request()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        // A publication that cannot claim the public name must leave no staging
        // file behind. The refused create above returns before staging runs, so
        // the collision itself is exercised directly here.
        let prepared = PreparedHistory::write(&path, b"staged\n").unwrap();
        let staging = prepared.path.clone();
        assert!(prepared.publish(&path).is_err());
        assert!(
            !staging.exists(),
            "failed publication must remove its staging file"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        clean(&path);
    }
    #[test]
    fn abandoned_staging_history_is_removed_without_a_public_file() {
        let directory =
            std::env::temp_dir().join(format!("eden-store-drop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("history.jsonl");
        let prepared = PreparedHistory::write(&path, b"staged\n").unwrap();
        let staging = prepared.path.clone();
        assert!(staging.is_file() && !path.exists());
        drop(prepared);
        assert!(
            !staging.exists(),
            "abandoned staging must not survive as private history"
        );
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
        std::fs::remove_dir_all(directory).unwrap();
    }
    fn clean(path: &std::path::Path) {
        let mut lock = path.as_os_str().to_owned();
        lock.push(".lock");
        std::fs::remove_file(path).unwrap();
        let _ = std::fs::remove_file(lock);
    }
}
