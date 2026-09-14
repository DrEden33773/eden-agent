//! Serial local public history, with an exclusive OS file lock and sync receipts.
use eden_plugin_sdk::{
    Package,
    protocol::{Descriptor, Fault, coding::*},
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
                let mut records = vec![];
                let file = if let Some(path) = path {
                    let mut file = OpenOptions::new()
                        .create(true)
                        .truncate(false)
                        .read(true)
                        .write(true)
                        .open(path)
                        .map_err(io)?;
                    file.try_lock().map_err(|e| {
                        fault(format!("history already owned or lock unavailable: {e}"))
                    })?;
                    let mut bytes = vec![];
                    file.read_to_end(&mut bytes).map_err(io)?;
                    records = decode_records(&bytes)?;
                    if records.first().is_some_and(|r| r.session_id != session_id) {
                        return Err(fault("session identity mismatch"));
                    }
                    Some(file)
                } else {
                    None
                };
                self.file = file;
                self.records = records;
                self.id = session_id;
                self.opened = true;
            }
            StoreRequest::Append {
                run_id,
                kind,
                payload,
            } => {
                if !self.opened || self.failed {
                    return Err(fault("store unavailable"));
                }
                let record = Record {
                    schema_version: 1,
                    session_id: self.id,
                    sequence: self.records.len() as u64 + 1,
                    run_id,
                    kind,
                    payload,
                };
                let mut bytes = serde_json::to_vec(&record).map_err(|e| fault(e.to_string()))?;
                bytes.push(b'\n');
                if let Some(file) = &mut self.file {
                    // Once any write fails, later writes must not turn an uncertain tail into a receipt.
                    if let Err(error) = file
                        .seek(std::io::SeekFrom::End(0))
                        .and_then(|_| file.write_all(&bytes))
                        .and_then(|_| file.sync_all())
                    {
                        self.failed = true;
                        return Err(io(error));
                    }
                }
                self.records.push(record);
            }
            StoreRequest::Read => {
                if !self.opened {
                    return Err(fault("store not open"));
                }
            }
            StoreRequest::Close => {
                self.file.take();
                self.opened = false;
            }
        }
        Ok(StoreReply {
            session_id: self.id,
            sequence: self.records.len() as u64,
            records: self.records.clone(),
        })
    }
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
        std::fs::remove_file(path).unwrap();
    }
}
