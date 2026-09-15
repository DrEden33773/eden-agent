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
    records: Vec<Record>,
    id: u64,
    failed: bool,
}
impl Store {
    fn handle(&mut self, request: StoreRequest) -> Result<StoreReply, Fault> {
        match request {
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
                            "legacy history is read-only; explicit migration to a new v2 file is required",
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
                    payload: serde_json::json!({"target":target,"branch":branch}),
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
        self.available()?;
        let (mut parent_id, branch) = branch_state(&self.records)?;
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
        self.commit(pending)
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
        fault(format!(
            "history already owned or lock unavailable: {error}"
        ))
    })?;
    Ok(lock)
}

fn publish_new(path: &std::path::Path, bytes: &[u8]) -> Result<(), Fault> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| fault("destination parent unavailable"))?;
    let (temporary, mut file) = loop {
        let temporary = parent.join(format!(
            ".eden-history-{}-{}.tmp",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io(error)),
        }
    };
    let write = file.write_all(bytes).and_then(|_| file.sync_all());
    drop(file);
    let result = write.and_then(|_| std::fs::hard_link(&temporary, path));
    let cleanup = std::fs::remove_file(&temporary);
    result.map_err(io)?;
    cleanup.map_err(io)?;
    // Persist the new directory entry where directory sync is available.
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(io)?;
    Ok(())
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
                payload: json!({"path":"a"}),
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
        assert_eq!(
            second.handle(open()).unwrap_err().code,
            "PersistenceFailure"
        );
        let receipt = first
            .handle(StoreRequest::Append {
                run_id: 1,
                kind: "tool_intent".into(),
                payload: json!({"name":"write"}),
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
        let bytes = b"{\"schema_version\":1,\"session_id\":7,\"sequence\":1,\"run_id\":1,\"kind\":\"user\",\"payload\":{}}\n";
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
                            payload: json!({"target":999})
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
        clean(&path);
    }
    fn clean(path: &std::path::Path) {
        let mut lock = path.as_os_str().to_owned();
        lock.push(".lock");
        std::fs::remove_file(path).unwrap();
        let _ = std::fs::remove_file(lock);
    }
}
