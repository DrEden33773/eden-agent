//! Independent SDK-only coding role replacements used by the installed verifier.
use eden_plugin_sdk::tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use eden_plugin_sdk::{
    CallContext, Package,
    protocol::{
        Descriptor, Fault,
        coding::*,
        history::{active_path, branch_state, encode_transaction, validate_records},
    },
    serde_json::{self, Value, json},
};
use std::{
    fs::File,
    io::{Read, Write},
    sync::{Arc, Mutex},
};

fn fault(e: impl std::fmt::Display) -> Fault {
    Fault::new("AuthorFailure", "coding-replacements", e.to_string())
}
fn message(text: String) -> Item {
    Item::Message {
        role: "assistant".into(),
        content: vec![Block::Text { text }],
    }
}
async fn provider(input: ModelInput, _: CallContext) -> Result<ModelReply, Fault> {
    let result = input.items.iter().rev().find_map(|item| match item {
        Item::ToolResult { result, .. } => Some(result),
        _ => None,
    });
    let items = match result {
        Some(result) => vec![message(format!(
            "Independent provider observed: {}",
            result.text
        ))],
        None if input.items.iter().any(|item| matches!(item, Item::Message {content,..} if content.iter().any(|block| matches!(block,Block::Text {text} if text.contains("AUTHOR_TOOL_COMPLETED=author-write") || text.contains("AUTHOR_TOOL_UNCERTAIN=author-write"))))) => vec![message("Independent provider retained the earlier tool outcome; no replay".into())],
        None => vec![Item::ToolCall {
            call_id: "author-write".into(),
            name: "write".into(),
            arguments:
                json!({"path":"author-created.txt","content":"independent provider wrote this\n"})
                    .to_string(),
        }],
    };
    Ok(ModelReply {
        items,
        usage: json!({"author":true}),
    })
}
fn project(records: &[Record]) -> Result<Vec<Item>, Fault> {
    let path = active_path(records)?;
    project_path(&path, records)
}

fn project_path(path: &[Record], records: &[Record]) -> Result<Vec<Item>, Fault> {
    let compact = path.iter().rev().find(|record| record.kind == "compaction");
    let mut items = vec![];
    if let Some(record) = compact {
        items.push(message(
            record.payload["summary"]
                .as_str()
                .ok_or_else(|| fault("summary missing"))?
                .into(),
        ));
    }
    for record in path {
        if compact.is_some_and(|compact| record.sequence <= compact.sequence) {
            continue;
        }
        match record.kind.as_str() {
            "message" | "tool_intent" | "tool_result" | "provider_state" => {
                items.push(serde_json::from_value(record.payload.clone()).map_err(fault)?)
            }
            "branch_summary" => items.push(message(
                record.payload["summary"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
            )),
            "queue_delivered" => {
                let id = &record.payload["id"];
                let last = records.iter().rev().find(|other| {
                    matches!(
                        other.kind.as_str(),
                        "queue_delivered" | "queue_returned" | "queue_consumed"
                    ) && other.payload["id"] == *id
                });
                if last.is_some_and(|last| {
                    last.kind == "queue_consumed" || last.sequence == record.sequence
                }) {
                    let entry: QueueEntry =
                        serde_json::from_value(record.payload.clone()).map_err(fault)?;
                    items.push(Item::Message {
                        role: "user".into(),
                        content: entry.content,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(items)
}

fn summary(records: &[Record]) -> Result<Value, Fault> {
    let path = active_path(records)?;
    summarize_items(&path, project(records)?)
}

fn branch_summary(records: &[Record]) -> Result<Value, Fault> {
    let mut payload = summarize_items(records, project_path(records, records)?)?;
    payload["origin_ids"] = json!(
        records
            .iter()
            .map(|record| record.sequence)
            .collect::<Vec<_>>()
    );
    payload["origin_session"] = json!(records.first().map(|record| record.session_id));
    Ok(payload)
}

fn summarize_items(path: &[Record], items: Vec<Item>) -> Result<Value, Fault> {
    let mut goals = vec![];
    let mut observations = vec![];
    let mut completed = std::collections::BTreeSet::new();
    let mut intents = std::collections::BTreeSet::new();
    for item in items {
        match item {
            Item::Message { role, content } => {
                let text: Vec<_> = content
                    .into_iter()
                    .filter_map(|block| {
                        if let Block::Text { text } = block {
                            Some(text)
                        } else {
                            None
                        }
                    })
                    .collect();
                if role == "user" {
                    goals.extend(text);
                } else {
                    observations.extend(text);
                }
            }
            Item::ToolCall { call_id, .. } => {
                intents.insert(call_id);
            }
            Item::ToolResult { call_id, result } => {
                observations.push(format!("AUTHOR_TOOL_COMPLETED={call_id}: {}", result.text));
                completed.insert(call_id);
            }
            Item::ProviderState { .. } => {}
        }
    }
    let uncertainties: Vec<_> = intents.difference(&completed).cloned().collect();
    for call_id in &uncertainties {
        observations.push(format!("AUTHOR_TOOL_UNCERTAIN={call_id}; do not replay"));
    }
    Ok(
        json!({"summary":format!("INDEPENDENT_SUMMARY\nGoals and constraints: {}\nObserved progress: {}",goals.join("; "),observations.join("; ")),"first_kept":0,"source_ids":path.iter().map(|record|record.sequence).collect::<Vec<_>>(),"uncertainties":uncertainties,"author":"coding-replacements"}),
    )
}

async fn context(input: ContextInput, cx: CallContext) -> Result<ModelInput, Fault> {
    let path = if input.action == "branch_summary" {
        input.records.clone()
    } else {
        active_path(&input.records)?
    };
    let states = path
        .iter()
        .filter(|record| record.kind == "extension_state")
        .map(|record| serde_json::from_value(record.payload.clone()).map_err(fault))
        .collect::<Result<Vec<ExtensionState>, Fault>>()?;
    let extension = if states.is_empty() {
        vec![]
    } else {
        cx.call::<_, InterpretReply>(INTERPRETER, &InterpretRequest { states })
            .await?
            .items
    };
    let records = if input.action == "compact" || input.action == "branch_summary" {
        let kind = if input.action == "compact" {
            "compaction"
        } else {
            "branch_summary"
        };
        let receipt: StoreReply = cx
            .call(
                STORE,
                &StoreRequest::Append {
                    run_id: cx.run_id(),
                    kind: kind.into(),
                    payload: if input.action == "branch_summary" {
                        branch_summary(&input.records)?
                    } else {
                        summary(&input.records)?
                    },
                },
            )
            .await?;
        receipt.records
    } else {
        input.records
    };
    let mut items = vec![Item::Message {
        role: "system".into(),
        content: vec![Block::Text {
            text: "INDEPENDENT_CONTEXT_MARKER".into(),
        }],
    }];
    items.extend(project(&records)?);
    items.extend(input.items);
    items.extend(extension);
    Ok(ModelInput {
        max_output_tokens: None,
        items,
        tools: vec![ToolDefinition {
            name: "write".into(),
            description: "Independent author write schema".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
        }],
    })
}

fn interpret(input: InterpretRequest) -> Result<InterpretReply, Fault> {
    let mut items = vec![];
    for state in input.states {
        if state.namespace == "author.counter" && state.version == 1 {
            let count = state
                .value
                .get("count")
                .and_then(Value::as_i64)
                .ok_or_else(|| fault("author.counter v1 needs integer count"))?;
            items.push(message(format!("AUTHOR_STATE={count}")));
        } else if state.required {
            return Err(Fault::new(
                "MissingInterpreter",
                "coding-replacements",
                format!(
                    "unsupported required state {} v{}",
                    state.namespace, state.version
                ),
            ));
        }
    }
    Ok(InterpretReply { items })
}

fn migrate(input: MigrateRequest, fail: bool, false_claim: bool) -> Result<MigrateReply, Fault> {
    if fail {
        return Err(fault("injected migration failure"));
    }
    let mut states = input.states;
    let mut preserved = vec![];
    for state in &mut states {
        if state.namespace == "author.counter" && state.version == 0 {
            let count = state
                .value
                .as_i64()
                .or_else(|| state.value.get("value").and_then(Value::as_i64))
                .ok_or_else(|| fault("author.counter v0 needs integer value"))?;
            if !false_claim {
                let mut fields = state.value.as_object().cloned().unwrap_or_default();
                if fields.contains_key("count") {
                    return Err(fault("v0 count field conflicts with v1 migration"));
                }
                fields.remove("value");
                fields.insert("count".into(), json!(count));
                state.value = Value::Object(fields);
                state.version = 1;
            }
            preserved.push("author.counter: numeric value translated to v1 count".into());
        } else if state.namespace == "author.counter" && state.version == 1 {
            interpret(InterpretRequest {
                states: vec![state.clone()],
            })?;
            preserved.push("author.counter v1 preserved".into());
        } else if state.required {
            return Err(fault("required state has no author migration"));
        } else {
            preserved.push(format!(
                "optional {} v{} preserved without interpretation",
                state.namespace, state.version
            ));
        }
    }
    Ok(MigrateReply {
        states,
        preserved,
        losses: vec![],
    })
}

async fn tool(input: ToolRequest, _: CallContext) -> Result<ToolResult, Fault> {
    let path = std::path::Path::new(&input.cwd).join("author-tool-effect.txt");
    std::fs::write(path, format!("{}:{}", input.name, input.call_id)).map_err(fault)?;
    Ok(ToolResult {
        text: "INDEPENDENT_TOOL_RESULT".into(),
        exit_code: Some(0),
        truncated: false,
        error: None,
    })
}
#[derive(Default)]
struct Storage {
    file: Option<File>,
    writer_lock: Option<File>,
    records: Vec<Record>,
    id: u64,
    open: bool,
    failed: bool,
    fail_kind: Option<String>,
}
impl Storage {
    fn request(&mut self, request: StoreRequest) -> Result<StoreReply, Fault> {
        match request {
            StoreRequest::Open { path, session_id } => {
                if self.open {
                    return Err(fault("already open"));
                }
                let (file, writer_lock, records) = if let Some(path) = path {
                    let path = std::path::Path::new(&path);
                    let lock = lock_history(path)?;
                    let mut file = File::options()
                        .create(true)
                        .truncate(false)
                        .read(true)
                        .append(true)
                        .open(path)
                        .map_err(fault)?;
                    let mut bytes = vec![];
                    file.read_to_end(&mut bytes).map_err(fault)?;
                    let records = decode_records(&bytes)?;
                    if records.first().is_some_and(|record| {
                        record.session_id != session_id || record.schema_version != 2
                    }) {
                        return Err(fault("identity mismatch or explicit migration required"));
                    }
                    (Some(file), Some(lock), records)
                } else {
                    (None, None, vec![])
                };
                self.file = file;
                self.writer_lock = writer_lock;
                self.records = records;
                self.id = session_id;
                self.open = true;
                self.failed = false;
            }
            StoreRequest::Create {
                path,
                session_id,
                records,
            } => {
                if self.open {
                    return Err(fault("already open"));
                }
                validate_records(&records)?;
                if records
                    .iter()
                    .any(|record| record.schema_version != 2 || record.session_id != session_id)
                {
                    return Err(fault("new history identity or schema mismatch"));
                }
                self.check_failure(&records)?;
                let bytes = encode_transaction(&records)?;
                let path = std::path::Path::new(&path);
                let lock = lock_history(path)?;
                PreparedHistory::write(path, &bytes)?.publish(path)?;
                let file = File::options()
                    .read(true)
                    .append(true)
                    .open(path)
                    .map_err(fault)?;
                self.file = Some(file);
                self.writer_lock = Some(lock);
                self.records = records;
                self.id = session_id;
                self.open = true;
                self.failed = false;
            }
            StoreRequest::Append {
                run_id,
                kind,
                payload,
            } => self.append(run_id, vec![RecordDraft { kind, payload }])?,
            StoreRequest::AppendBatch { run_id, entries } => self.append(run_id, entries)?,
            StoreRequest::Navigate { target, branch } => {
                self.available()?;
                self.commit(vec![Record {
                    schema_version: 2,
                    session_id: self.id,
                    sequence: self.records.len() as u64 + 1,
                    run_id: 0,
                    parent_id: Some(target),
                    branch: branch.clone(),
                    kind: "branch_selected".into(),
                    payload: json!({"target":target,"branch":branch}),
                }])?;
            }
            StoreRequest::Read => {
                if !self.open {
                    return Err(fault("closed"));
                }
            }
            StoreRequest::Close => {
                self.file.take();
                self.writer_lock.take();
                self.open = false;
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
        if !self.open || self.failed {
            Err(fault("store unavailable"))
        } else {
            Ok(())
        }
    }
    fn append(&mut self, run_id: u64, entries: Vec<RecordDraft>) -> Result<(), Fault> {
        self.available()?;
        let (mut parent_id, branch) = branch_state(&self.records)?;
        let mut pending = vec![];
        for entry in entries {
            if entry.kind == "branch_selected" {
                return Err(fault("use Navigate"));
            }
            let sequence = self.records.len() as u64 + pending.len() as u64 + 1;
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
    fn check_failure(&self, records: &[Record]) -> Result<(), Fault> {
        if let Some(record) = records
            .iter()
            .find(|record| self.fail_kind.as_deref() == Some(record.kind.as_str()))
        {
            return Err(Fault::new(
                "PersistenceFailure",
                "independent-store",
                format!("injected {} append failure before write", record.kind),
            ));
        }
        Ok(())
    }
    fn commit(&mut self, pending: Vec<Record>) -> Result<(), Fault> {
        self.check_failure(&pending)?;
        let mut records = self.records.clone();
        records.extend(pending.iter().cloned());
        validate_records(&records)?;
        let bytes = encode_transaction(&pending)?;
        if let Some(file) = &mut self.file
            && let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all())
        {
            self.failed = true;
            return Err(fault(error));
        }
        self.records = records;
        Ok(())
    }
}
/// A synced private staging file whose final public name is still absent.
/// Drop removes staging on write, publication, and caller failures.
struct PreparedHistory {
    path: std::path::PathBuf,
    cleanup: bool,
}
impl PreparedHistory {
    fn write(destination: &std::path::Path, bytes: &[u8]) -> Result<Self, Fault> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        let (prepared, mut file) = loop {
            let path = parent.join(format!(
                ".eden-author-history-{}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match File::options().create_new(true).write(true).open(&path) {
                Ok(file) => {
                    break (
                        Self {
                            path,
                            cleanup: true,
                        },
                        file,
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(fault(error)),
            }
        };
        let result = file.write_all(bytes).and_then(|_| file.sync_all());
        // Close before cleanup/publication so the same ownership works on Windows.
        drop(file);
        result.map_err(fault)?;
        Ok(prepared)
    }
    fn publish(mut self, destination: &std::path::Path) -> Result<(), Fault> {
        // A hard link exposes the complete synced bytes atomically and refuses
        // any competing destination; rename could silently replace that file.
        std::fs::hard_link(&self.path, destination).map_err(fault)?;
        std::fs::remove_file(&self.path).map_err(fault)?;
        self.cleanup = false;
        #[cfg(unix)]
        {
            let parent = destination
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new("."));
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(fault)?;
        }
        Ok(())
    }
}
impl Drop for PreparedHistory {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn lock_history(path: &std::path::Path) -> Result<File, Fault> {
    let canonical = if path.exists() {
        std::fs::canonicalize(path).map_err(fault)?
    } else {
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::canonicalize(parent)
            .map_err(fault)?
            .join(path.file_name().ok_or_else(|| fault("filename required"))?)
    };
    let mut lock_path = canonical.into_os_string();
    lock_path.push(".lock");
    let lock = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(fault)?;
    lock.try_lock().map_err(fault)?;
    Ok(lock)
}

fn descriptor() -> Descriptor {
    Descriptor {
        package: "coding-replacements".into(),
        version: "0.1.0".into(),
        provides: vec![
            PROVIDER.into(),
            CONTEXT.into(),
            TOOL.into(),
            STORE.into(),
            INTERPRETER.into(),
            MIGRATOR.into(),
        ],
    }
}
fn create(config: Value) -> Result<Package, Fault> {
    let open_gate = config
        .get("open_gate")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let storage = Arc::new(Mutex::new(Storage {
        fail_kind: config
            .get("fail_kind")
            .and_then(Value::as_str)
            .map(str::to_owned),
        ..Storage::default()
    }));
    let fail_migration = config
        .get("fail_migration")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let false_claim = config
        .get("false_claim")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(Package::new("coding-replacements")
        .service(PROVIDER, provider)
        .service(CONTEXT, context)
        .service(TOOL, tool)
        .service(STORE, move |request: StoreRequest, _| {
            let opening = matches!(request, StoreRequest::Open { .. });
            let closing = matches!(request, StoreRequest::Close);
            let gate = open_gate.clone();
            let result = storage
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .request(request);
            async move {
                let receipt = result?;
                if let Some(address) = gate {
                    if opening {
                        let mut stream = TcpStream::connect(&address).await.map_err(fault)?;
                        // Open has already acquired the public-history writer lock.
                        stream.write_all(b"open-ready\n").await.map_err(fault)?;
                        let mut release = [0];
                        stream.read_exact(&mut release).await.map_err(fault)?;
                    } else if closing {
                        let mut stream = TcpStream::connect(&address).await.map_err(fault)?;
                        // Close has already dropped the separate writer lock.
                        stream.write_all(b"store-closed\n").await.map_err(fault)?;
                    }
                }
                Ok(receipt)
            }
        })
        .service(INTERPRETER, |request: InterpretRequest, _| async move {
            interpret(request)
        })
        .service(MIGRATOR, move |request: MigrateRequest, _| async move {
            migrate(request, fail_migration, false_claim)
        }))
}
eden_plugin_sdk::export_plugin!(descriptor, create);

#[cfg(test)]
mod tests {
    use super::*;
    fn state(version: u32, value: Value) -> ExtensionState {
        ExtensionState {
            namespace: "author.counter".into(),
            version,
            required: true,
            summary: "counter".into(),
            value,
        }
    }
    #[test]
    fn detached_branch_summary_retains_its_source_ids() {
        let records = vec![Record {
            schema_version: 2,
            session_id: 4,
            sequence: 9,
            run_id: 3,
            parent_id: Some(3),
            branch: "old".into(),
            kind: "message".into(),
            payload: json!(Item::Message {
                role: "user".into(),
                content: vec![Block::Text {
                    text: "detached requirement".into()
                }]
            }),
        }];
        let result = branch_summary(&records).unwrap();
        assert_eq!(result["origin_ids"], json!([9]));
        assert!(
            result["summary"]
                .as_str()
                .unwrap()
                .contains("detached requirement")
        );
    }

    #[test]
    fn migration_preview_and_apply_translate_real_state_identically() {
        let states = vec![state(0, json!({"value":17}))];
        let preview = migrate(
            MigrateRequest {
                states: states.clone(),
                apply: false,
            },
            false,
            false,
        )
        .unwrap();
        let applied = migrate(
            MigrateRequest {
                states,
                apply: true,
            },
            false,
            false,
        )
        .unwrap();
        assert_eq!(json!(preview), json!(applied));
        assert_eq!(preview.states[0].version, 1);
        assert_eq!(preview.states[0].value, json!({"count":17}));
        let result = interpret(InterpretRequest {
            states: preview.states,
        })
        .unwrap();
        assert_eq!(result.items, vec![message("AUTHOR_STATE=17".into())]);
        assert!(preview.losses.is_empty());
    }
    #[test]
    fn migration_preserves_additional_extension_fields() {
        let result = migrate(
            MigrateRequest {
                states: vec![state(0, json!({"value":3,"label":"keep"}))],
                apply: true,
            },
            false,
            false,
        )
        .unwrap();
        assert_eq!(result.states[0].value, json!({"count":3,"label":"keep"}));
        assert!(result.losses.is_empty());
    }

    #[test]
    fn unsupported_required_state_is_rejected_before_projection() {
        assert!(
            interpret(InterpretRequest {
                states: vec![state(0, json!(3))]
            })
            .is_err()
        );
        let mut unknown = state(1, json!({"count":3}));
        unknown.namespace = "unknown.counter".into();
        assert!(
            interpret(InterpretRequest {
                states: vec![unknown.clone()]
            })
            .is_err()
        );
        unknown.required = false;
        assert!(
            interpret(InterpretRequest {
                states: vec![unknown]
            })
            .unwrap()
            .items
            .is_empty()
        );
    }
    #[test]
    fn create_publishes_complete_history_and_preserves_colliding_destination() {
        let directory = std::env::temp_dir().join(format!(
            "eden-author-publish-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("history.jsonl");
        let records = vec![Record {
            schema_version: 2,
            session_id: 91,
            sequence: 1,
            run_id: 0,
            parent_id: None,
            branch: "main".into(),
            kind: "session".into(),
            payload: json!({"cwd":"isolated"}),
        }];
        let bytes = encode_transaction(&records).unwrap();
        let prepared = PreparedHistory::write(&path, &bytes).unwrap();
        let temporary = prepared.path.clone();
        assert!(
            !path.exists(),
            "final name must remain absent before publication"
        );
        assert_eq!(std::fs::read(&temporary).unwrap(), bytes);
        std::fs::write(&path, b"existing destination").unwrap();
        assert!(prepared.publish(&path).is_err());
        assert!(
            !temporary.exists(),
            "failed publication must clean temporary history"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"existing destination");
        let request = || StoreRequest::Create {
            path: path.to_string_lossy().into(),
            session_id: 91,
            records: records.clone(),
        };
        assert!(Storage::default().request(request()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing destination");
        std::fs::remove_file(&path).unwrap();
        let mut storage = Storage::default();
        let receipt = storage.request(request()).unwrap();
        assert_eq!(receipt.sequence, 1);
        assert_eq!(
            json!(decode_records(&std::fs::read(&path).unwrap()).unwrap()),
            json!(records)
        );
        storage.request(StoreRequest::Close).unwrap();
        assert_eq!(
            std::fs::read_dir(&directory).unwrap().count(),
            2,
            "only public history and writer lock remain"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_batch_does_not_commit_an_earlier_draft() {
        let mut store = Storage {
            fail_kind: Some("tool_intent".into()),
            ..Storage::default()
        };
        store
            .request(StoreRequest::Open {
                path: None,
                session_id: 8,
            })
            .unwrap();
        assert!(
            store
                .request(StoreRequest::AppendBatch {
                    run_id: 1,
                    entries: vec![
                        RecordDraft {
                            kind: "message".into(),
                            payload: json!({})
                        },
                        RecordDraft {
                            kind: "tool_intent".into(),
                            payload: json!({})
                        }
                    ]
                })
                .is_err()
        );
        assert!(
            store
                .request(StoreRequest::Read)
                .unwrap()
                .records
                .is_empty()
        );
    }
    #[test]
    fn independent_compaction_changes_input_and_keeps_source_records() {
        let mut store = Storage::default();
        store
            .request(StoreRequest::Open {
                path: None,
                session_id: 8,
            })
            .unwrap();
        for text in ["Build a counter", "Keep the counter positive"] {
            store
                .request(StoreRequest::Append {
                    run_id: 1,
                    kind: "message".into(),
                    payload: json!(Item::Message {
                        role: "user".into(),
                        content: vec![Block::Text { text: text.into() }]
                    }),
                })
                .unwrap();
        }
        let before = project(&store.records).unwrap();
        let payload = summary(&store.records).unwrap();
        store
            .request(StoreRequest::Append {
                run_id: 1,
                kind: "compaction".into(),
                payload,
            })
            .unwrap();
        let after = project(&store.records).unwrap();
        assert_eq!(before.len(), 2);
        assert_eq!(after.len(), 1);
        assert_ne!(before, after);
        assert_eq!(store.records.len(), 3);
        assert!(json!(after).to_string().contains("Build a counter"));
    }
}
