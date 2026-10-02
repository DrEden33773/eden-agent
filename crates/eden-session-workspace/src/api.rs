//! In-process commands carry intent, not JSON-RPC routing or process ownership instructions.
use crate::{Adapter, Cleanup, Lifecycle, OpenOutcome, sessions::Server};
use eden_protocol::Fault;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::mpsc;

/// Flexible presentation payloads retain their meaning without sharing a wire envelope.
#[derive(Clone, Copy, Debug)]
#[allow(missing_docs)]
pub enum ViewEventKind {
    Update,
    Title,
    Context,
    Models,
    PromptComplete,
    Queue,
    CatalogProgress,
    Connection,
}

/// Ordered view stream. The consumer acknowledges the barrier only after preceding updates
/// enter its view state; load completion never guesses whether another queue has drained.
#[allow(missing_docs)]
pub enum ViewEvent {
    Notification { kind: ViewEventKind, data: Value },
    Barrier(tokio::sync::oneshot::Sender<()>),
}

/// Startup chooses an explicit target; opening the application never guesses a recent history.
#[allow(missing_docs)]
pub enum OpenTarget {
    New { persist: bool },
    History { path: PathBuf, reading: bool },
    Endpoint(PathBuf),
}

/// Operations on an attached view. Flexible blocks are presentation payloads, not lifecycle commands.
#[allow(missing_docs)]
pub enum SessionOperation {
    Initialize,
    Authenticate,
    /// Prepare replay without selecting it. The consumer applies the ready reply
    /// before sending `Request::Activate`, or cancels this view's opening attempt.
    Load,
    Prompt {
        blocks: Vec<Value>,
        prompt_id: Option<String>,
    },
    Cancel {
        prompt_id: Option<String>,
    },
    Compact,
    PromptHistory,
    Rename {
        title: String,
    },
    SelectModel {
        model_id: String,
        effort: Option<String>,
    },
    DefaultModel {
        model_id: String,
        effort: Option<String>,
    },
}

/// A single authority owns view selection and every history management operation.
#[allow(missing_docs)]
pub enum Request {
    View {
        view: Option<String>,
        operation: SessionOperation,
    },
    New,
    /// Commit a prepared view after the consumer has applied its ready result.
    Activate {
        view: String,
    },
    /// Abandon one opening attempt without cancelling work in either Session.
    CancelLoad {
        view: String,
    },
    Catalog(Value),
    Form(Value),
}

/// A consumer's Session workspace owns its attachment and pending cleanup. Hosts own accepted work.
pub struct SessionWorkspace {
    server: Arc<Server>,
    initial: String,
    next: AtomicU64,
}
impl SessionWorkspace {
    /// Prepare the explicit initial target and an ordered presentation stream.
    pub async fn start(
        context: Lifecycle,
        target: OpenTarget,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<ViewEvent>), Fault> {
        let opened = match target {
            OpenTarget::New { persist } => context.create(persist).await?,
            OpenTarget::History { path, reading } => context.open(&path, reading).await?,
            OpenTarget::Endpoint(path) => context.attach(&path, None).await?,
        };
        let mut ownership = context.preparing_consumer(&opened);
        let (output, events) = mpsc::unbounded_channel();
        let mut adapter =
            match Adapter::new(opened.endpoint.clone(), output, Some(opened.view_key())).await {
                Ok(adapter) => adapter,
                Err(error) => {
                    context.failed_consumer(&opened).await?;
                    ownership.disarm();
                    return Err(error);
                }
            };
        adapter.opened = Some(opened.clone());
        adapter.reading_diagnostic = opened.diagnostic.clone();
        let initial = adapter.identity.clone();
        ownership.disarm();
        Ok((
            Arc::new(Self {
                server: Arc::new(Server::new(Arc::new(adapter), context, opened)),
                initial,
                next: AtomicU64::new(0),
            }),
            events,
        ))
    }

    /// The initial view key is opaque to callers; it is not a history locator.
    pub fn initial_view(&self) -> &str {
        &self.initial
    }

    /// Execute a typed user intent. A successful load follows replay publication in the event stream.
    pub async fn request(&self, request: Request) -> Result<Value, Fault> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (method, view) = match &request {
            Request::New => ("session/new", None),
            Request::Activate { view } => ("session/activate", Some(view.as_str())),
            Request::CancelLoad { view } => ("session/cancel_load", Some(view.as_str())),
            Request::Catalog(_) => ("session/catalog", None),
            Request::Form(_) => ("eden/ui", None),
            Request::View { view, operation } => (
                match operation {
                    SessionOperation::Initialize => "initialize",
                    SessionOperation::Authenticate => "authenticate",
                    SessionOperation::Load => "session/load",
                    SessionOperation::Prompt { .. } => "session/prompt",
                    SessionOperation::Cancel { .. } => "session/cancel",
                    SessionOperation::Compact => "session/compact",
                    SessionOperation::PromptHistory => "session/prompt_history",
                    SessionOperation::Rename { .. } => "session/rename",
                    SessionOperation::SelectModel { .. } => "session/set_model",
                    SessionOperation::DefaultModel { .. } => "model/default",
                },
                view.as_deref(),
            ),
        };
        crate::trace(json!({ "direction": "in", "id": id, "method": method, "session": view }));
        let result = async {
            match request {
                Request::Catalog(query) => self.server.list(&query).await,
                Request::Form(input) => self.server.forms.dispatch(&self.server, &input).await,
                Request::Activate { view } => {
                    self.server.activate(&view).await?;
                    Ok(json!({}))
                }
                Request::CancelLoad { view } => {
                    self.server.cancel_load(&view).await?;
                    Ok(json!({}))
                }
                Request::New => {
                    let root = self.server.root().await;
                    let id = self.server.create(&root).await?;
                    // The consumer must bind this identity before requesting projection.
                    // Publishing here would acknowledge updates that have no view yet.
                    Ok(json!({ "sessionId": id }))
                }
                Request::View { view, operation } => {
                    let loading = matches!(operation, SessionOperation::Load);
                    let identity = view
                        .clone()
                        .unwrap_or(self.server.root().await.identity.clone());
                    if loading {
                        self.server.begin_load(&identity).await?;
                    }
                    let mut result = async {
                        let target = self.server.target(view.as_deref(), loading).await?;
                        let reply = target.execute(&operation).await?;
                        if loading {
                            self.server.presented().await?;
                        }
                        Ok(reply)
                    }
                    .await;
                    if loading && !self.server.prepare_load(&identity, result.is_ok()).await {
                        result = Err(Fault::new(
                            "Cancelled",
                            "session-workspace",
                            "session opening cancelled",
                        ));
                    }
                    result
                }
            }
        }
        .await;
        crate::trace(json!({
            "direction": "out",
            "id": id,
            "error": result.as_ref().err().map(|error| &error.code),
            "session": result.as_ref().ok().and_then(|r| r["sessionId"].as_str()),
            "run": result
                .as_ref()
                .ok()
                .and_then(|r| r["_meta"]["edenRunId"].as_u64()),
            "prompt": result
                .as_ref()
                .ok()
                .and_then(|r| r["_meta"]["promptId"].as_str()),
        }));
        self.server.follow_all().await;
        result
    }

    /// Finish consumer-owned cleanup. A draft with no accepted work is closed; saved hosts detach.
    pub async fn close(&self) -> Result<(), Fault> {
        let _timing = eden_protocol::latency::Span::new("frontend.cleanup", 0, 0);
        self.server.cancel_catalog();
        let mut error = self.server.finish_startup_cleanup().await.err();
        self.server.stop_followers().await;
        self.server.forms.close_all(&self.server).await;
        let adapters = self.server.all().await;
        // A host can back several view generations. Release this consumer's full
        // attachment set before transferring cleanup once to each owned host.
        for adapter in &adapters {
            let lease = adapter.lease.swap(0, Ordering::AcqRel);
            if lease != 0 {
                let _ = adapter.client.detach(lease).await;
            }
        }
        let mut released = std::collections::BTreeSet::new();
        for adapter in adapters {
            if let Some(opened) = &adapter.opened {
                let owned = opened.cleanup == Cleanup::OwnedReader
                    || opened.outcome == OpenOutcome::Created;
                if !owned || !released.insert((opened.endpoint.clone(), opened.instance.clone())) {
                    continue;
                }
                if opened.cleanup == Cleanup::OwnedReader {
                    if let Err(fault) = self.server.close_reader(opened).await {
                        error.get_or_insert(fault);
                    }
                } else if opened.outcome == OpenOutcome::Created
                    && let Err(fault) = self.server.close_unused(opened).await
                {
                    error.get_or_insert(fault);
                }
            }
        }
        error.map_or(Ok(()), Err)
    }
}
