//! Shared persistent and memory session API for the CLI and Rust consumers.
use eden_kernel::{Events, Kernel};
use eden_plugin_sdk::Cancellation;
use eden_protocol::{AGENT_LOOP, Request, RunInput, coding as c};
pub use eden_protocol::{Event, Fault, Outcome, Terminal};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
struct State {
    closed: bool,
    next: u64,
    active: Option<(u64, Cancellation)>,
    terminals: BTreeMap<u64, Terminal>,
    shutdown_result: Option<Result<(), Fault>>,
}
struct Inner {
    id: u64,
    kernel: Kernel,
    events: Arc<Events>,
    state: Mutex<State>,
    settled: tokio::sync::Notify,
    shutdown: tokio::sync::Mutex<()>,
    coding: bool,
    cwd: String,
}
/// Cloneable client for one session. Explicitly await shutdown before dropping the runtime.
#[derive(Clone)]
pub struct Session(Arc<Inner>);
/// Explicit session resources. `history: None` is memory-only.
#[derive(Clone, Debug)]
pub struct SessionOptions {
    pub cwd: std::path::PathBuf,
    pub history: Option<std::path::PathBuf>,
}
impl Session {
    pub async fn open(composition: impl AsRef<Path>) -> Result<Self, Fault> {
        let cwd = std::env::current_dir()
            .map_err(|e| Fault::new("InvalidInput", "cwd", e.to_string()))?;
        Self::open_with(composition, SessionOptions { cwd, history: None }).await
    }
    pub async fn open_with(
        composition: impl AsRef<Path>,
        options: SessionOptions,
    ) -> Result<Self, Fault> {
        let composition = composition.as_ref().to_owned();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = Self::open_owned(composition, options).await;
            let _ = sender.send(SessionDelivery(Some(result)));
        });
        let mut delivery = receiver
            .await
            .map_err(|e| Fault::new("Unavailable", "session-open", e.to_string()))?;
        delivery
            .0
            .take()
            .ok_or_else(|| Fault::new("Unavailable", "session-open", "missing delivery"))?
    }
    async fn open_owned(
        composition: std::path::PathBuf,
        options: SessionOptions,
    ) -> Result<Self, Fault> {
        let cwd = std::fs::canonicalize(&options.cwd)
            .map_err(|e| Fault::new("InvalidInput", "cwd", e.to_string()))?;
        if !cwd.is_dir() {
            return Err(Fault::new("InvalidInput", "cwd", "cwd must be a directory"));
        }
        let cwd = cwd.to_string_lossy().into_owned();
        let previous = match &options.history {
            Some(path) if path.exists() => eden_kernel::history::read(path)?,
            _ => vec![],
        };
        let id = previous.first().map(|r| r.session_id).unwrap_or_else(|| {
            let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64;
            time.wrapping_add(NEXT_SESSION.fetch_add(1, Ordering::Relaxed))
        });
        let next = previous.iter().map(|r| r.run_id).max().unwrap_or(0) + 1;
        let events = Events::new(id);
        let kernel = Kernel::load(composition.as_ref(), id, events.clone()).await?;
        let coding = kernel.role(c::LOOP).is_ok();
        if options.history.is_some() && !coding {
            kernel.shutdown().await?;
            return Err(Fault::new(
                "Unsupported",
                "session",
                "controlled skeleton supports only memory sessions",
            ));
        }
        let session = Self(Arc::new(Inner {
            id,
            kernel,
            events,
            state: Mutex::new(State {
                closed: false,
                next,
                active: None,
                terminals: BTreeMap::new(),
                shutdown_result: None,
            }),
            settled: tokio::sync::Notify::new(),
            shutdown: tokio::sync::Mutex::new(()),
            coding,
            cwd: cwd.clone(),
        }));
        if coding {
            let setup = async {
                let reply: c::StoreReply = session.service(0, c::STORE, &c::StoreRequest::Open { path: options.history.map(|p| p.to_string_lossy().into_owned()), session_id: id }).await?;
                session.0.state.lock().unwrap_or_else(|e| e.into_inner()).next = reply.records.iter().map(|r| r.run_id).max().unwrap_or(0) + 1;
                let binding = session.0.kernel.composition();
                let identity = serde_json::json!({"cwd":cwd, "roles":binding.roles, "packages":binding.packages.iter().map(|p| &p.descriptor).collect::<Vec<_>>()});
                if let Some(record) = reply.records.first() {
                    if record.kind != "session" || record.payload != identity { return Err(Fault::new("Unavailable", "session", "history cwd or composition differs; reopen with the original binding")); }
                } else { session.commit(0,"session",identity).await?; }
                Ok::<_,Fault>(())
            }.await;
            if let Err(error) = setup {
                let _ = session.shutdown().await;
                return Err(error);
            }
        }
        Ok(session)
    }
    pub fn id(&self) -> u64 {
        self.0.id
    }
    /// Accept a run without waiting for its execution. A second active submission is rejected.
    pub fn submit(&self, prompt: impl Into<String>) -> Result<u64, Fault> {
        self.submit_blocks(vec![c::Block::Text {
            text: prompt.into(),
        }])
    }
    pub fn submit_blocks(&self, content: Vec<c::Block>) -> Result<u64, Fault> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed || state.active.is_some() {
            return Err(Fault::new(
                "Unavailable",
                "session",
                "session closed or already running",
            ));
        }
        let run_id = state.next;
        state.next += 1;
        let cancel = Cancellation::default();
        state.active = Some((run_id, cancel.clone()));
        self.0
            .events
            .push(run_id, "accepted", serde_json::Value::Null);
        let request = Request {
            session_id: self.id(),
            run_id,
            contract: if self.0.coding { c::LOOP } else { AGENT_LOOP }.into(),
            payload: if self.0.coding {
                serde_json::json!(c::RunInput {
                    cwd: self.0.cwd.clone(),
                    content
                })
            } else {
                let prompt = content
                    .into_iter()
                    .filter_map(|b| match b {
                        c::Block::Text { text } => Some(text),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                serde_json::json!(RunInput { prompt })
            },
        };
        let session = self.clone();
        tokio::spawn(async move {
            let mut terminal = session.0.kernel.invoke(request, cancel).await;
            if session.0.coding
                && let Err(error) = session
                    .commit(run_id, "terminal", serde_json::json!(terminal))
                    .await
            {
                // Preserve an earlier failure and report persistence failure separately.
                terminal.cleanup_errors.push(error);
            }
            let mut state = session.0.state.lock().unwrap_or_else(|e| e.into_inner());
            state.terminals.insert(run_id, terminal.clone());
            session
                .0
                .events
                .push(run_id, "settled", serde_json::json!(terminal));
            state.active = None;
            session.0.settled.notify_waiters();
        });
        Ok(run_id)
    }
    /// Signal cancellation. The settled event and wait() result are the completion barrier.
    pub fn cancel(&self, run_id: u64) -> Result<(), Fault> {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((active, cancel)) = &state.active
            && *active == run_id
        {
            cancel.cancel();
            self.0
                .events
                .push(run_id, "cancel_requested", serde_json::Value::Null);
            return Ok(());
        }
        if state.terminals.contains_key(&run_id) {
            return Ok(());
        }
        Err(Fault::new("InvalidInput", "session", "unknown run"))
    }
    pub fn inspect(&self, run_id: u64) -> Option<Terminal> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .terminals
            .get(&run_id)
            .cloned()
    }
    pub async fn wait(&self, run_id: u64) -> Result<Terminal, Fault> {
        loop {
            let settled = self.0.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            {
                let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(result) = state.terminals.get(&run_id) {
                    return Ok(result.clone());
                }
                if state.active.as_ref().is_none_or(|(id, _)| *id != run_id) {
                    return Err(Fault::new("InvalidInput", "session", "unknown run"));
                }
            }
            settled.await;
        }
    }
    pub fn events(&self) -> Vec<Event> {
        self.0.events.snapshot()
    }
    pub async fn events_after(&self, sequence: u64) -> Vec<Event> {
        self.0.events.after(sequence).await
    }
    /// Useful for low-level native integration; the returned proxy retains its generation gate.
    pub fn role(&self, contract: &str) -> Result<Arc<eden_kernel::NativeInstance>, Fault> {
        self.0.kernel.role(contract)
    }
    /// Close admission, cancel and settle the active run, then destroy plugin-owned runtimes.
    pub async fn shutdown(&self) -> Result<(), Fault> {
        let session = self.clone();
        tokio::spawn(async move { session.shutdown_owned().await })
            .await
            .map_err(|e| Fault::new("CleanupFailure", "shutdown", e.to_string()))?
    }
    async fn shutdown_owned(&self) -> Result<(), Fault> {
        let _owner = self.0.shutdown.lock().await;
        if let Some(result) = &self
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown_result
        {
            return result.clone();
        }
        let active = {
            let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            state.active.as_ref().map(|(id, cancel)| {
                cancel.cancel();
                *id
            })
        };
        if let Some(run_id) = active {
            self.wait(run_id).await?;
        }
        let close = if self.0.coding {
            self.service::<_, c::StoreReply>(0, c::STORE, &c::StoreRequest::Close)
                .await
                .map(|_| ())
        } else {
            Ok(())
        };
        let stopped = self.0.kernel.shutdown().await;
        let result = close.and(stopped);
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown_result = Some(result.clone());
        result
    }
    async fn service<I: serde::Serialize, O: serde::de::DeserializeOwned>(
        &self,
        run_id: u64,
        role: &str,
        input: &I,
    ) -> Result<O, Fault> {
        let payload = serde_json::to_value(input)
            .map_err(|e| Fault::new("InvalidInput", "session", e.to_string()))?;
        let value = self
            .0
            .kernel
            .invoke(
                Request {
                    session_id: self.id(),
                    run_id,
                    contract: role.into(),
                    payload,
                },
                Cancellation::default(),
            )
            .await
            .into_result()?;
        serde_json::from_value(value)
            .map_err(|e| Fault::new("InvalidInput", "session", e.to_string()))
    }
    async fn commit(
        &self,
        run_id: u64,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<(), Fault> {
        let receipt: c::StoreReply = self
            .service(
                run_id,
                c::STORE,
                &c::StoreRequest::Append {
                    run_id,
                    kind: kind.into(),
                    payload,
                },
            )
            .await?;
        self.0.events.push(
            run_id,
            "committed",
            serde_json::json!({"sequence":receipt.sequence,"kind":kind}),
        );
        Ok(())
    }
    /// Read committed public history through the selected storage role.
    pub async fn history(&self) -> Result<Vec<c::Record>, Fault> {
        Ok(self
            .service::<_, c::StoreReply>(0, c::STORE, &c::StoreRequest::Read)
            .await?
            .records)
    }
    /// Enqueue one input. Delivery happens at the default loop's next boundary.
    pub async fn enqueue(
        &self,
        kind: &str,
        content: Vec<c::Block>,
    ) -> Result<c::QueueEntry, Fault> {
        let run_id = {
            let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed {
                return Err(Fault::new("Unavailable", "session", "session closed"));
            }
            state.active.as_ref().map(|(id, _)| *id).unwrap_or(0)
        };
        let mut entries: Vec<c::QueueEntry> = self
            .service(
                run_id,
                c::QUEUE,
                &c::QueueRequest::Enqueue {
                    kind: kind.into(),
                    content,
                },
            )
            .await?;
        entries
            .pop()
            .ok_or_else(|| Fault::new("Unavailable", "queue", "missing acceptance"))
    }
    pub async fn queued(&self) -> Result<Vec<c::QueueEntry>, Fault> {
        self.service(0, c::QUEUE, &c::QueueRequest::Inspect).await
    }
}

struct SessionDelivery(Option<Result<Session, Fault>>);
impl Drop for SessionDelivery {
    fn drop(&mut self) {
        if let Some(Ok(session)) = self.0.take() {
            tokio::spawn(async move {
                let _ = session.shutdown().await;
            });
        }
    }
}
