//! Independent SDK-only coding role replacements used by the installed verifier.
use eden_plugin_sdk::tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use eden_plugin_sdk::{
    CallContext, Package,
    protocol::{Descriptor, Fault, coding::*},
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
async fn context(input: ContextInput, _: CallContext) -> Result<ModelInput, Fault> {
    let mut items = vec![Item::Message {
        role: "system".into(),
        content: vec![Block::Text {
            text: "INDEPENDENT_CONTEXT_MARKER".into(),
        }],
    }];
    items.extend(input.items);
    Ok(ModelInput {
        items,
        tools: vec![ToolDefinition {
            name: "write".into(),
            description: "Independent author write schema".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
        }],
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
    fail_kind: Option<String>,
}
impl Storage {
    fn request(&mut self, request: StoreRequest) -> Result<StoreReply, Fault> {
        match request {
            StoreRequest::Open { path, session_id } => {
                if self.open {
                    return Err(fault("already open"));
                }
                self.id = session_id;
                self.records.clear();
                if let Some(path) = path {
                    let mut file = File::options()
                        .create(true)
                        .truncate(false)
                        .read(true)
                        .append(true)
                        .open(&path)
                        .map_err(fault)?;
                    let mut lock_path = std::fs::canonicalize(&path)
                        .map_err(fault)?
                        .into_os_string();
                    lock_path.push(".lock");
                    let writer_lock = File::options()
                        .create(true)
                        .truncate(false)
                        .read(true)
                        .write(true)
                        .open(lock_path)
                        .map_err(fault)?;
                    writer_lock.try_lock().map_err(fault)?;
                    let mut bytes = vec![];
                    file.read_to_end(&mut bytes).map_err(fault)?;
                    self.records = decode_records(&bytes)?;
                    self.writer_lock = Some(writer_lock);
                    self.file = Some(file);
                }
                self.open = true;
            }
            StoreRequest::Append {
                run_id,
                kind,
                payload,
            } => {
                if !self.open {
                    return Err(fault("closed"));
                }
                if self.fail_kind.as_deref() == Some(kind.as_str()) {
                    return Err(Fault::new(
                        "PersistenceFailure",
                        "independent-store",
                        format!("injected {kind} append failure before write"),
                    ));
                }
                let record = Record {
                    schema_version: 1,
                    session_id: self.id,
                    sequence: self.records.len() as u64 + 1,
                    run_id,
                    kind,
                    payload,
                };
                if let Some(file) = &mut self.file {
                    let bytes = format!("{}\n", serde_json::to_string(&record).map_err(fault)?);
                    file.write_all(bytes.as_bytes()).map_err(fault)?;
                    file.sync_all().map_err(fault)?;
                }
                self.records.push(record);
            }
            StoreRequest::Read => {}
            StoreRequest::Close => {
                self.file.take();
                self.writer_lock.take();
                self.open = false;
            }
        }
        Ok(StoreReply {
            session_id: self.id,
            sequence: self.records.len() as u64,
            records: self.records.clone(),
        })
    }
}
fn descriptor() -> Descriptor {
    Descriptor {
        package: "coding-replacements".into(),
        version: "0.1.0".into(),
        provides: vec![PROVIDER.into(), CONTEXT.into(), TOOL.into(), STORE.into()],
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
        }))
}
eden_plugin_sdk::export_plugin!(descriptor, create);
