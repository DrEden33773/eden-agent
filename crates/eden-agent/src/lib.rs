//! In-memory session API used by the CLI and Rust consumers.
use eden_kernel::{Events, Kernel};
use eden_plugin_sdk::Cancellation;
use eden_protocol::{AGENT_LOOP, Request, RunInput};
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
}
struct Inner {
    id: u64,
    kernel: Kernel,
    events: Arc<Events>,
    state: Mutex<State>,
    settled: tokio::sync::Notify,
    shutdown: tokio::sync::Mutex<()>,
}
/// Cloneable client for one session. Explicitly await shutdown before dropping the runtime.
#[derive(Clone)]
pub struct Session(Arc<Inner>);
impl Session {
    pub async fn open(composition: impl AsRef<Path>) -> Result<Self, Fault> {
        let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
        let events = Events::new(id);
        let kernel = Kernel::load(composition.as_ref(), id, events.clone()).await?;
        Ok(Self(Arc::new(Inner {
            id,
            kernel,
            events,
            state: Mutex::new(State {
                closed: false,
                next: 1,
                active: None,
                terminals: BTreeMap::new(),
            }),
            settled: tokio::sync::Notify::new(),
            shutdown: tokio::sync::Mutex::new(()),
        })))
    }
    pub fn id(&self) -> u64 {
        self.0.id
    }
    /// Accept a run without waiting for its execution. A second active submission is rejected.
    pub fn submit(&self, prompt: impl Into<String>) -> Result<u64, Fault> {
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
            contract: AGENT_LOOP.into(),
            payload: serde_json::json!(RunInput {
                prompt: prompt.into()
            }),
        };
        let session = self.clone();
        tokio::spawn(async move {
            let terminal = session.0.kernel.invoke(request, cancel).await;
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
        self.0.kernel.shutdown().await
    }
}
