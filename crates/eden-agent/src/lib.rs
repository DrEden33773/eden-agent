//! Shared persistent and memory session API for the CLI and Rust consumers.
use eden_kernel::{Events, Kernel};
mod attempts;
mod composition;
mod generation;
mod models;
mod workspace_setup;
use eden_plugin_sdk::Cancellation;
use eden_protocol::{AGENT_LOOP, Request, RunInput, coding as c};
pub use eden_protocol::{Event, Fault, Outcome, Terminal};
pub use eden_workspace::{Workspace, WorkspaceOptions, save_trust};
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
    management: bool,
    pending_inputs: usize,
    terminals: BTreeMap<u64, Terminal>,
    shutdown_result: Option<Result<(), Fault>>,
}
struct Inner {
    id: u64,
    kernel: generation::Generation,
    workspace_options: WorkspaceOptions,
    history_path: Option<std::path::PathBuf>,
    offline_records: Mutex<Vec<c::Record>>,
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
    /// The directory the session runs in and resolves relative paths against.
    pub cwd: std::path::PathBuf,
    /// The history file to open or create; `None` keeps the session in memory
    /// and writes no file on any exit path.
    pub history: Option<std::path::PathBuf>,
}
impl Session {
    /// Open a memory-only session in the current directory. This is the
    /// convenience form; no history file is created on any exit path.
    pub async fn open(composition: impl AsRef<Path>) -> Result<Self, Fault> {
        let cwd = std::env::current_dir()
            .map_err(|e| Fault::new("InvalidInput", "cwd", e.to_string()))?;
        Self::open_with(composition, SessionOptions { cwd, history: None }).await
    }
    /// Open with explicit resources under the default workspace options.
    pub async fn open_with(
        composition: impl AsRef<Path>,
        options: SessionOptions,
    ) -> Result<Self, Fault> {
        Self::open_with_workspace(composition, options, WorkspaceOptions::default()).await
    }
    /// Open with explicit resources and explicit workspace options, which is
    /// how the CLI hands over its trust decision and setting overrides.
    pub async fn open_with_workspace(
        composition: impl AsRef<Path>,
        options: SessionOptions,
        workspace: WorkspaceOptions,
    ) -> Result<Self, Fault> {
        Self::open_with_policy(composition.as_ref().to_owned(), options, workspace, false).await
    }
    /// Explicitly bind saved history to a different installed composition without running it.
    pub async fn open_rebound(
        composition: impl AsRef<Path>,
        options: SessionOptions,
        workspace: WorkspaceOptions,
    ) -> Result<Self, Fault> {
        Self::open_with_policy(composition.as_ref().to_owned(), options, workspace, true).await
    }
    async fn open_with_policy(
        composition: std::path::PathBuf,
        options: SessionOptions,
        workspace: WorkspaceOptions,
        rebind: bool,
    ) -> Result<Self, Fault> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = Self::open_owned(composition, options, workspace, rebind).await;
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
        workspace_options: WorkspaceOptions,
        rebind: bool,
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
        // A composition switch changes packages, not the immutable session cwd.
        // Copy/migration creates a new header when relocating a saved session.
        if rebind
            && previous
                .first()
                .is_some_and(|record| record.payload["cwd"] != cwd)
        {
            return Err(Fault::new(
                "InvalidInput",
                "cwd",
                "cannot change saved cwd during a composition switch; use a session copy or \
                 migration",
            ));
        }
        if previous
            .first()
            .is_some_and(|record| record.schema_version == 1)
        {
            return Err(Fault::new(
                "IncompatibleContract",
                "session",
                "v1 history is readable; preview an explicit session upgrade before continuing",
            ));
        }
        let id = previous
            .first()
            .map(|r| r.session_id)
            .unwrap_or_else(new_session_id);
        let next = previous.iter().map(|r| r.run_id).max().unwrap_or(0) + 1;
        let events = Events::new(id);
        let selected = workspace_setup::prepare(
            &composition,
            &cwd,
            &workspace_options,
            &events,
            options.history.as_deref(),
        )?;
        let desired = composition::binding(&selected, &cwd)?;
        if !rebind
            && previous
                .iter()
                .rev()
                .find(|r| r.kind == "composition_lock")
                .is_some_and(|saved| !composition::equivalent(&saved.payload, &desired))
        {
            return Err(Fault::new(
                "Unavailable",
                "composition",
                "saved package binding differs; use an explicit session switch",
            ));
        }
        let kernel = Kernel::load_resolved(
            selected,
            composition.parent().unwrap_or(Path::new(".")),
            id,
            events.clone(),
        )
        .await?;
        let coding = kernel.role(c::LOOP).is_ok();
        if options.history.is_some() && !coding {
            let mut error = Fault::new(
                "Unsupported",
                "session",
                "controlled skeleton supports only memory sessions",
            );
            if let Err(cleanup) = kernel.shutdown().await {
                error
                    .message
                    .push_str(&format!("; rollback cleanup: {cleanup}"));
            }
            return Err(error);
        }
        let session = Self(Arc::new(Inner {
            id,
            kernel: generation::Generation::new(kernel),
            workspace_options,
            history_path: options.history.clone(),
            offline_records: Mutex::new(previous.clone()),
            events,
            state: Mutex::new(State {
                closed: false,
                next,
                active: None,
                management: false,
                pending_inputs: 0,
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
                let reply: c::StoreReply = session
                    .service(
                        0,
                        c::STORE,
                        &c::StoreRequest::Open {
                            path: options.history.map(|p| p.to_string_lossy().into_owned()),
                            session_id: id,
                        },
                    )
                    .await?;
                session
                    .0
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .next = reply.records.iter().map(|r| r.run_id).max().unwrap_or(0) + 1;
                let binding = session.0.kernel.composition();
                let identity = serde_json::json!({
                    "cwd": cwd,
                    "roles": binding.roles,
                    "packages": binding
                        .packages
                        .iter()
                        .map(|p| &p.descriptor)
                        .collect::<Vec<_>>(),
                });
                let locked = desired;
                let saved_lock = reply
                    .records
                    .iter()
                    .rev()
                    .find(|r| r.kind == "composition_lock");
                if let Some(saved) = saved_lock
                    && !rebind
                    && !composition::equivalent(&saved.payload, &locked)
                {
                    return Err(Fault::new(
                        "Unavailable",
                        "composition",
                        "saved package binding differs; explicitly switch the saved session \
                         composition",
                    ));
                }
                if let Some(record) = reply.records.first() {
                    if record.kind != "session"
                        || (!rebind
                            && saved_lock.is_none()
                            && !compatible_binding(&record.payload, &identity))
                    {
                        return Err(Fault::new(
                            "Unavailable",
                            "session",
                            "history cwd or composition differs; reopen with the original binding",
                        ));
                    }
                } else {
                    session.commit(0, "session", identity).await?;
                }
                let _: Vec<c::QueueEntry> = session
                    .service(0, c::QUEUE, &c::QueueRequest::Restore)
                    .await?;
                workspace_setup::register(&session, &binding, true)?;
                if rebind
                    || saved_lock.is_none_or(|saved| {
                        saved.payload["library_locations"] != locked["library_locations"]
                    })
                {
                    session.commit(0, "composition_lock", locked).await?;
                }
                workspace_setup::register(&session, &binding, false)?;
                Ok::<_, Fault>(())
            }
            .await;
            if let Err(mut error) = setup {
                if let Err(cleanup) = session.shutdown().await {
                    error
                        .message
                        .push_str(&format!("; rollback cleanup: {cleanup}"));
                }
                return Err(error);
            }
        }
        Ok(session)
    }
    /// The identity saved history records share, and which copies replace.
    pub fn id(&self) -> u64 {
        self.0.id
    }
    /// Accept a run without waiting for its execution. A second active submission is rejected.
    pub fn submit(&self, prompt: impl Into<String>) -> Result<u64, Fault> {
        self.submit_blocks(vec![c::Block::Text {
            text: prompt.into(),
        }])
    }
    /// Accept multimodal content as one run. The blocks are carried as given,
    /// so an attachment does not depend on its source file afterwards.
    pub fn submit_blocks(&self, content: Vec<c::Block>) -> Result<u64, Fault> {
        self.start_loop(content, false)
    }
    /// Explicitly continue from committed context and restored queues, without a new prompt.
    pub fn resume(&self) -> Result<u64, Fault> {
        self.start_loop(vec![], true)
    }
    fn start_loop(&self, content: Vec<c::Block>, resume: bool) -> Result<u64, Fault> {
        self.0.kernel.get()?;
        let mut payload = if self.0.coding {
            serde_json::json!(c::RunInput {
                target: None,
                resume,
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
        };
        let role = if self.0.coding { c::LOOP } else { AGENT_LOOP };
        self.start(false, move |session, run_id, cancel| async move {
            if session.0.coding {
                match session.freeze_model(run_id).await {
                    Ok(target) => payload["target"] = serde_json::json!(target),
                    Err(error) => return Terminal::failed(error),
                }
            }
            session
                .0
                .kernel
                .invoke(
                    Request {
                        session_id: session.id(),
                        run_id,
                        contract: role.into(),
                        payload,
                    },
                    cancel,
                )
                .await
        })
    }
    fn start<F, Fut>(&self, management: bool, operation: F) -> Result<u64, Fault>
    where
        F: FnOnce(Session, u64, Cancellation) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Terminal> + Send + 'static,
    {
        self.start_with_public_result(management, Clone::clone, operation)
    }
    fn start_with_public_result<F, Fut>(
        &self,
        management: bool,
        public_result: fn(&Terminal) -> Terminal,
        operation: F,
    ) -> Result<u64, Fault>
    where
        F: FnOnce(Session, u64, Cancellation) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Terminal> + Send + 'static,
    {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed || state.active.is_some() || state.pending_inputs > 0 {
            return Err(Fault::new(
                "Unavailable",
                "session",
                "session closed or already running",
            ));
        }
        let run_id = state.next;
        state.next = state
            .next
            .checked_add(1)
            .ok_or_else(|| Fault::new("Unavailable", "session", "run identity exhausted"))?;
        let cancel = Cancellation::default();
        state.active = Some((run_id, cancel.clone()));
        state.management = management;
        self.0.events.push(
            run_id,
            "accepted",
            serde_json::json!({ "management": management }),
        );
        let session = self.clone();
        tokio::spawn(async move {
            let mut terminal = operation(session.clone(), run_id, cancel).await;
            if session.0.coding && session.0.kernel.available() {
                // Persistence runs in fresh admission after the cancelled invocation
                // has drained; partial provider output can never execute here.
                match session
                    .service::<_, c::StoreReply>(run_id, c::STORE, &c::StoreRequest::Read)
                    .await
                {
                    Ok(history) => {
                        for attempt in attempts::records(
                            &session.events(),
                            run_id,
                            &public_result(&terminal),
                            &history.records,
                        ) {
                            if let Err(error) =
                                session.commit(run_id, "model_attempt", attempt).await
                            {
                                terminal.cleanup_errors.push(error);
                            }
                        }
                    }
                    Err(error) => terminal.cleanup_errors.push(error),
                }
                if let Err(error) = session
                    .service::<_, Vec<c::QueueEntry>>(run_id, c::QUEUE, &c::QueueRequest::Restore)
                    .await
                {
                    terminal.cleanup_errors.push(error);
                }
                if let Err(error) = session
                    .commit(
                        run_id,
                        "terminal",
                        serde_json::json!(public_result(&terminal)),
                    )
                    .await
                {
                    terminal.cleanup_errors.push(error);
                }
            }
            let mut state = session.0.state.lock().unwrap_or_else(|e| e.into_inner());
            state.terminals.insert(run_id, terminal.clone());
            session.0.events.push(
                run_id,
                "settled",
                serde_json::json!(public_result(&terminal)),
            );
            state.active = None;
            state.management = false;
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
    /// The settled result of a run, without waiting for one that is still active.
    /// Authentication results include private transient UI data; do not log or persist them.
    pub fn inspect(&self, run_id: u64) -> Option<Terminal> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .terminals
            .get(&run_id)
            .cloned()
    }
    /// Wait for a run to settle and return its terminal. The registration
    /// happens before the state is read, so a run that settles in between is
    /// reported rather than waited for.
    /// Authentication results include private transient UI data; do not log or persist them.
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
    /// Every event this session has published, in order.
    pub fn events(&self) -> Vec<Event> {
        self.0.events.snapshot()
    }
    /// Wait for the events after `sequence`, for a reader that keeps a cursor.
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
        loop {
            let changed = self.0.settled.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self
                .0
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pending_inputs
                == 0
            {
                break;
            }
            changed.await;
        }
        let close = if self.0.coding && self.0.kernel.available() {
            self.service::<_, c::StoreReply>(0, c::STORE, &c::StoreRequest::Close)
                .await
                .map(|_| ())
        } else {
            Ok(())
        };
        let stopped = self.0.kernel.shutdown().await;
        let result = match (close, stopped) {
            (Err(mut error), Err(cleanup)) => {
                error
                    .message
                    .push_str(&format!("; instance cleanup: {cleanup}"));
                Err(error)
            }
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        };
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
            serde_json::json!({ "sequence": receipt.sequence, "kind": kind }),
        );
        Ok(())
    }
    /// Read committed public history through the selected storage role.
    pub async fn history(&self) -> Result<Vec<c::Record>, Fault> {
        if !self.0.kernel.available() {
            return match &self.0.history_path {
                Some(path) => eden_kernel::history::read(path),
                None => Ok(self
                    .0
                    .offline_records
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()),
            };
        }
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
            let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed || state.management {
                return Err(Fault::new(
                    "Unavailable",
                    "session",
                    "session closed or managing history",
                ));
            }
            state.pending_inputs += 1;
            state.active.as_ref().map(|(id, _)| *id).unwrap_or(0)
        };
        let session = self.clone();
        let kind = kind.to_owned();
        let owner = InputOwner(session.clone());
        tokio::spawn(async move {
            let _owner = owner;
            let mut entries: Vec<c::QueueEntry> = session
                .service(
                    run_id,
                    c::QUEUE,
                    &c::QueueRequest::Enqueue { kind, content },
                )
                .await?;
            entries
                .pop()
                .ok_or_else(|| Fault::new("Unavailable", "queue", "missing acceptance"))
        })
        .await
        .map_err(|e| Fault::new("Unavailable", "queue", e.to_string()))?
    }
    /// The pending submissions of the active branch, in delivery order.
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

/// Session copy and migration plans.
pub mod management;
pub use management::{CopyKind, CopyOptions, CopyPlan};

fn new_session_id() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    // Per-instance random hash keys avoid time-only collisions across processes.
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(NEXT_SESSION.fetch_add(1, Ordering::Relaxed));
    hasher.write_u32(std::process::id());
    hasher.finish()
}
fn compatible_binding(saved: &serde_json::Value, current: &serde_json::Value) -> bool {
    if saved["cwd"] != current["cwd"] || saved["roles"] != current["roles"] {
        return false;
    }
    // Selected role identities define the binding; unused package inventories and
    // compatible package-version changes are not historical state dependencies.
    true
}

struct InputOwner(Session);
impl Drop for InputOwner {
    fn drop(&mut self) {
        let mut state = self.0.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.pending_inputs -= 1;
        self.0.0.settled.notify_waiters();
    }
}
