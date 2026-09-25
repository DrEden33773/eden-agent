//! Typed Rust interfaces implemented by plugin authors.
use crate::{
    abi::{Bytes, HostApi, Reply},
    scope::{BoxFuture, Scope},
};
use eden_protocol::{
    self as p, Descriptor, Fault, ModelInput, ModelReply, Request, RunInput, Terminal,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{collections::BTreeMap, future::Future, sync::Arc};

/// An invocation's identity, managed scope and asynchronous service access.
#[derive(Clone)]
pub struct CallContext {
    /// The admission and cleanup owner of this operation.
    pub scope: Scope,
    pub(crate) host: HostApi,
    pub(crate) request: Request,
}
impl CallContext {
    /// The session this operation belongs to.
    pub fn session_id(&self) -> u64 {
        self.request.session_id
    }
    /// The run this operation belongs to.
    pub fn run_id(&self) -> u64 {
        self.request.run_id
    }
    /// Emit one observation during the active operation. An emission after the
    /// operation's scope has closed is rejected rather than delivered late.
    pub fn emit(&self, kind: &str, payload: Value) -> Result<(), Fault> {
        let event = Request {
            execution: self.request.execution.clone(),
            session_id: self.session_id(),
            run_id: self.run_id(),
            contract: kind.into(),
            payload,
        };
        let bytes = serde_json::to_vec(&event).map_err(serialization)?;
        // SAFETY: HostApi outlives the scope; the host copies the span during this call.
        self.scope.while_open(|| {
            // SAFETY: Holding scope admission prevents finalization and host destruction during emit.
            unsafe {
                (self.host.event)(self.host.context, Bytes::new(&bytes));
            }
        })
    }
    /// Call a selected service. The scope retains the bridge through cancellation and cleanup.
    pub async fn call<I: Serialize, O: DeserializeOwned>(
        &self,
        contract: &str,
        input: &I,
    ) -> Result<O, Fault> {
        let request = Request {
            execution: self.request.execution.clone(),
            session_id: self.session_id(),
            run_id: self.run_id(),
            contract: contract.into(),
            payload: serde_json::to_value(input).map_err(serialization)?,
        };
        let bytes = serde_json::to_vec(&request).map_err(serialization)?;
        let (sender, receiver) = tokio::sync::oneshot::channel::<Terminal>();
        let token = Box::into_raw(Box::new(sender)) as usize;
        let host = self.host;
        let cancel = self.scope.cancellation();
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        // Register before sending so closing admission cannot orphan an accepted host request.
        self.scope
            .spawn(async move {
                // SAFETY: The host borrows bytes and consumes token exactly once on acceptance.
                let id = unsafe {
                    (host.request)(
                        host.context,
                        Bytes::new(&bytes),
                        Reply {
                            context: token,
                            call: receive_terminal,
                        },
                    )
                };
                if id == 0 {
                    // SAFETY: A rejected request leaves the token with its allocating side.
                    drop(unsafe {
                        Box::from_raw(token as *mut tokio::sync::oneshot::Sender<Terminal>)
                    });
                    let _ = result_sender.send(Err(Fault::new(
                        "Unavailable",
                        "host",
                        "service request admission closed",
                    )));
                    return Ok(());
                }
                let mut receiver = receiver;
                let reply = tokio::select! {
                    biased;
                    reply = &mut receiver => reply,
                    _ = cancel.cancelled() => {
                        // SAFETY: The live host owns this request id; cancellation is idempotent.
                        unsafe {
                            (host.cancel)(host.context, id);
                        }
                        receiver.await
                    }
                };
                let result = reply
                    .map_err(|_| Fault::new("Unavailable", "host", "host completion lost"))
                    .and_then(Terminal::into_result);
                let _ = result_sender.send(result);
                Ok(())
            })
            .inspect_err(|_| {
                // SAFETY: spawn rejected without polling the future, so no host owns token.
                drop(unsafe {
                    Box::from_raw(token as *mut tokio::sync::oneshot::Sender<Terminal>)
                });
            })?;
        let value = result_receiver
            .await
            .map_err(|_| Fault::new("Unavailable", "bridge", "bridge lost"))??;
        serde_json::from_value(value).map_err(serialization)
    }
    /// Read host-authoritative paths and trust without requiring object-shaped factory config.
    pub async fn host_environment(&self) -> Result<Option<p::environment::HostEnvironment>, Fault> {
        self.call(p::runtime::HOST, &p::runtime::HostRequest::Environment)
            .await
    }
    /// Host-issued owner and generation; never derive ownership from package configuration.
    pub fn identity(&self) -> Option<&p::runtime::CallIdentity> {
        self.request.execution.as_ref()
    }
    /// Delegate to the remaining wrapper chain exactly once. A retained or consumed token fails.
    pub async fn delegate<I: Serialize, O: DeserializeOwned>(&self, input: &I) -> Result<O, Fault> {
        let token = self.identity().and_then(|id| id.next).ok_or_else(|| {
            Fault::new(
                "ExpiredContinuation",
                "runtime",
                "no continuation in this invocation",
            )
        })?;
        self.call(
            p::runtime::HOST,
            &p::runtime::HostRequest::Delegate {
                token,
                input: serde_json::to_value(input).map_err(serialization)?,
            },
        )
        .await
    }
    /// Resolve a service in an explicit descendant scope; siblings retain their own bindings.
    pub async fn call_in<I: Serialize, O: DeserializeOwned>(
        &self,
        scope: &str,
        contract: &str,
        input: &I,
    ) -> Result<O, Fault> {
        self.call(
            p::runtime::HOST,
            &p::runtime::HostRequest::Call {
                scope: scope.into(),
                contract: contract.into(),
                input: serde_json::to_value(input).map_err(serialization)?,
            },
        )
        .await
    }
    /// Register an instance-owned service callback. Its lifetime is independent of this turn.
    pub async fn submit_job<I: Serialize>(
        &self,
        contract: &str,
        input: &I,
    ) -> Result<p::runtime::JobStatus, Fault> {
        self.call(
            p::runtime::HOST,
            &p::runtime::HostRequest::Submit {
                contract: contract.into(),
                input: serde_json::to_value(input).map_err(serialization)?,
            },
        )
        .await
    }
    /// Observe registration or settled completion without waiting for a running job.
    pub async fn inspect_job(&self, job: u64) -> Result<p::runtime::JobStatus, Fault> {
        self.call(p::runtime::HOST, &p::runtime::HostRequest::Inspect { job })
            .await
    }
    /// Release a settled job receipt. At most 1024 receipts may be retained per instance;
    /// forgetting a running job fails so it cannot detach work from its owner.
    pub async fn forget_job(&self, job: u64) -> Result<(), Fault> {
        self.call(p::runtime::HOST, &p::runtime::HostRequest::Forget { job })
            .await
    }
    /// Signal cancellation without claiming cleanup has finished; use join_job for the barrier.
    pub async fn cancel_job(&self, job: u64) -> Result<(), Fault> {
        self.call(p::runtime::HOST, &p::runtime::HostRequest::Cancel { job })
            .await
    }
    /// Wait for the callback, children and cleanup. Cancelling this waiter does not abandon the job.
    pub async fn join_job(&self, job: u64) -> Result<p::runtime::JobStatus, Fault> {
        self.call(p::runtime::HOST, &p::runtime::HostRequest::Join { job })
            .await
    }
    /// Await ordered observations. Lag is explicit, and cancellation releases the subscription.
    pub async fn events_after(
        &self,
        after: u64,
        kinds: Vec<String>,
    ) -> Result<p::runtime::EventBatch, Fault> {
        self.call(
            p::runtime::HOST,
            &p::runtime::HostRequest::Events { after, kinds },
        )
        .await
    }
    /// Publish or replace an owner-scoped semantic view. The host derives the owner from
    /// the installed package callback, so a caller cannot claim another package's slot.
    pub async fn present(
        &self,
        view: p::presentation::View,
    ) -> Result<p::presentation::Revision, Fault> {
        self.call(
            p::presentation::HOST,
            &p::presentation::HostRequest::Publish {
                owner: String::new(),
                view,
            },
        )
        .await
    }
    /// Remove a live view belonging to this package.
    pub async fn remove_view(&self, id: impl Into<String>) -> Result<(), Fault> {
        self.call(
            p::presentation::HOST,
            &p::presentation::HostRequest::Remove {
                owner: String::new(),
                id: id.into(),
            },
        )
        .await
    }
    /// Call the context role for this loop's projected model input.
    pub async fn context(&self, input: &RunInput) -> Result<ModelInput, Fault> {
        self.call(p::CONTEXT, input).await
    }
    /// Call the model provider for a completed reply.
    pub async fn model(&self, input: &ModelInput) -> Result<ModelReply, Fault> {
        self.call(p::PROVIDER, input).await
    }
    /// Call the selected tool with the argument the loop chose.
    pub async fn tool(&self, input: &str) -> Result<String, Fault> {
        self.call(p::TOOL, &input).await
    }
}
// SAFETY: Only the ABI callback that receives the accepted request consumes this sender.
unsafe extern "C" fn receive_terminal(token: usize, bytes: Bytes) {
    // SAFETY: token was allocated in call(); accepted callbacks consume it once.
    let sender = unsafe { Box::from_raw(token as *mut tokio::sync::oneshot::Sender<Terminal>) };
    // SAFETY: The host keeps the completion span live during this synchronous callback.
    let terminal = unsafe { bytes.decode() }.unwrap_or_else(Terminal::failed);
    let _ = sender.send(terminal);
}
fn serialization(error: serde_json::Error) -> Fault {
    Fault::new("InvalidInput", "codec", error.to_string())
}

/// Loop policy owns service ordering and the final result.
pub trait AgentLoop: Send + Sync + 'static {
    /// Own the run: choose the order of the other roles and return the result text.
    fn run(
        &self,
        input: RunInput,
        context: CallContext,
    ) -> impl Future<Output = Result<String, Fault>> + Send;
}
/// Context strategy projects the input seen by the provider.
pub trait ContextStrategy: Send + Sync + 'static {
    /// Project the run into the model input this provider will receive.
    fn project(
        &self,
        input: RunInput,
        context: CallContext,
    ) -> impl Future<Output = Result<ModelInput, Fault>> + Send;
}
/// Provider consumes the context strategy's actual model input.
pub trait ModelProvider: Send + Sync + 'static {
    /// Answer one model input, optionally emitting deltas through the context.
    fn generate(
        &self,
        input: ModelInput,
        context: CallContext,
    ) -> impl Future<Output = Result<ModelReply, Fault>> + Send;
}
/// Tool consumes a loop-selected argument and returns its result.
pub trait Tool: Send + Sync + 'static {
    /// Execute one loop-selected argument and return its result text.
    fn execute(
        &self,
        input: String,
        context: CallContext,
    ) -> impl Future<Output = Result<String, Fault>> + Send;
}
type Handler = Arc<dyn Fn(Value, CallContext) -> BoxFuture<Result<Value, Fault>> + Send + Sync>;
/// One package may provide several independently selectable roles.
pub struct Package {
    pub(crate) descriptor: Descriptor,
    handlers: BTreeMap<String, Handler>,
}
impl Package {
    /// The contribution identity callers must supply again when reopening a saved session.
    pub fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }
    /// Start a package with its name, version `0.1.0` and no roles yet.
    pub fn new(name: &str) -> Self {
        Self {
            descriptor: Descriptor {
                package: name.into(),
                version: "0.1.0".into(),
                provides: vec![],
            },
            handlers: BTreeMap::new(),
        }
    }
    fn add(mut self, role: &str, handler: Handler) -> Self {
        self.descriptor.provides.push(role.into());
        self.handlers.insert(role.into(), handler);
        self
    }
    /// Register an explicitly versioned role with typed, serialized payloads.
    pub fn service<I, O, F, Fut>(self, contract: &str, handler: F) -> Self
    where
        I: DeserializeOwned + Send + 'static,
        O: Serialize + Send + 'static,
        F: Fn(I, CallContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, Fault>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        self.add(
            contract,
            Arc::new(move |input, context| {
                let handler = handler.clone();
                Box::pin(async move {
                    let input = serde_json::from_value(input).map_err(serialization)?;
                    serde_json::to_value(handler(input, context).await?).map_err(serialization)
                })
            }),
        )
    }
    /// Register the trait facade for one role contract. A new package registers
    /// the explicitly versioned contract with [`Package::service`] instead,
    /// because the facade fixes the contract this release ships.
    pub fn agent_loop(self, role: impl AgentLoop) -> Self {
        let role = Arc::new(role);
        self.add(
            p::AGENT_LOOP,
            Arc::new(move |input, context| {
                let role = role.clone();
                Box::pin(async move {
                    serde_json::to_value(
                        role.run(
                            serde_json::from_value(input).map_err(serialization)?,
                            context,
                        )
                        .await?,
                    )
                    .map_err(serialization)
                })
            }),
        )
    }
    /// Register a trait facade for the context role, for a package written
    /// against these facades rather than an explicitly versioned contract.
    pub fn context(self, role: impl ContextStrategy) -> Self {
        let role = Arc::new(role);
        self.add(
            p::CONTEXT,
            Arc::new(move |input, context| {
                let role = role.clone();
                Box::pin(async move {
                    serde_json::to_value(
                        role.project(
                            serde_json::from_value(input).map_err(serialization)?,
                            context,
                        )
                        .await?,
                    )
                    .map_err(serialization)
                })
            }),
        )
    }
    /// Register a trait facade for the provider role, for a package written
    /// against these facades rather than an explicitly versioned contract.
    pub fn provider(self, role: impl ModelProvider) -> Self {
        let role = Arc::new(role);
        self.add(
            p::PROVIDER,
            Arc::new(move |input, context| {
                let role = role.clone();
                Box::pin(async move {
                    serde_json::to_value(
                        role.generate(
                            serde_json::from_value(input).map_err(serialization)?,
                            context,
                        )
                        .await?,
                    )
                    .map_err(serialization)
                })
            }),
        )
    }
    /// Register a trait facade for the tool role, for a package written against
    /// these facades rather than an explicitly versioned contract.
    pub fn tool(self, role: impl Tool) -> Self {
        let role = Arc::new(role);
        self.add(
            p::TOOL,
            Arc::new(move |input, context| {
                let role = role.clone();
                Box::pin(async move {
                    serde_json::to_value(
                        role.execute(
                            serde_json::from_value(input).map_err(serialization)?,
                            context,
                        )
                        .await?,
                    )
                    .map_err(serialization)
                })
            }),
        )
    }
    pub(crate) async fn invoke(
        &self,
        input: Value,
        role: &str,
        context: CallContext,
    ) -> Result<Value, Fault> {
        let handler = self
            .handlers
            .get(role)
            .ok_or_else(|| Fault::new("MissingDependency", &self.descriptor.package, role))?;
        handler(input, context).await
    }
}
