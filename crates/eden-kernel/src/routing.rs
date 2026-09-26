//! Admission-bound calls, single-use continuations and independently owned jobs.
use super::*;
use p::runtime::{CallIdentity, EventBatch, HostRequest, InstanceIdentity, JobStatus};
use std::collections::BTreeSet;

pub(super) struct LiveCall {
    pub identity: CallIdentity,
    pub request: Request,
    pub chain: Vec<String>,
    pub index: usize,
    pub stack: Vec<(String, String)>,
    pub delegated: bool,
}
pub(super) struct Job {
    pub owner: InstanceIdentity,
    pub cancel: Cancellation,
    pub status: tokio::sync::watch::Sender<Option<Terminal>>,
}
pub(super) struct Router {
    pub reconfiguring: Mutex<BTreeSet<String>>,
    pub environment: Option<p::environment::HostEnvironment>,
    pub session_id: u64,
    pub events: Arc<Events>,
    pub open: AtomicBool,
    pub next: AtomicU64,
    pub requests: Mutex<BTreeMap<u64, Cancellation>>,
    pub workers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    pub runtime: tokio::runtime::Handle,
    pub graph: graph::Graph,
    pub instances: Mutex<BTreeMap<String, Arc<Managed>>>,
    pub calls: Mutex<BTreeMap<u64, LiveCall>>,
    pub jobs: Mutex<BTreeMap<u64, Arc<Job>>>,
}
impl Router {
    pub fn unit(&self, id: &str) -> Result<Arc<Managed>, Fault> {
        self.instances
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| {
                Fault::new(
                    "Unavailable",
                    "runtime",
                    format!("instance {id} is not published"),
                )
            })
    }
    pub async fn invoke(self: &Arc<Self>, request: Request, cancel: Cancellation) -> Terminal {
        if request.session_id != self.session_id || !self.open.load(Ordering::Acquire) {
            return Terminal::failed(Fault::new(
                "Unavailable",
                "router",
                "session closed or foreign session",
            ));
        }
        let scope = request
            .execution
            .as_ref()
            .map_or("", |c| c.scope.as_str())
            .to_owned();
        let binding = match self.graph.binding(&scope, &request.contract) {
            Ok(binding) => binding,
            Err(error) => return Terminal::failed(error),
        };
        let ids: Vec<_> = binding
            .wrappers
            .iter()
            .chain(std::iter::once(&binding.tail))
            .cloned()
            .collect();
        if let Err(error) = self.admit(&ids) {
            return Terminal::failed(error);
        }
        self.route(request, scope, cancel, vec![], None).await
    }
    async fn route(
        self: &Arc<Self>,
        request: Request,
        scope: String,
        cancel: Cancellation,
        stack: Vec<(String, String)>,
        job: Option<u64>,
    ) -> Terminal {
        let binding = match self.graph.binding(&scope, &request.contract) {
            Ok(b) => b,
            Err(e) => return Terminal::failed(e),
        };
        let chain = binding
            .wrappers
            .iter()
            .chain(std::iter::once(&binding.tail))
            .cloned()
            .collect();
        self.dispatch(request, scope, chain, 0, cancel, stack, job)
            .await
    }
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch(
        self: &Arc<Self>,
        request: Request,
        scope: String,
        chain: Vec<String>,
        index: usize,
        cancel: Cancellation,
        stack: Vec<(String, String)>,
        job: Option<u64>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Terminal> + Send + '_>> {
        self.dispatch_expected(request, scope, chain, index, cancel, stack, job, None)
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dispatch_expected(
        self: &Arc<Self>,
        mut request: Request,
        scope: String,
        chain: Vec<String>,
        index: usize,
        cancel: Cancellation,
        mut stack: Vec<(String, String)>,
        job: Option<u64>,
        expected: Option<&InstanceIdentity>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Terminal> + Send + '_>> {
        let router = self.clone();
        // Capture the publication before queueing work: admitted old calls must not drift into
        // the replacement while the runtime waits to poll their worker.
        let unit = self.unit(&chain[index]).and_then(|unit| {
            if expected.is_some_and(|expected| *expected != unit.identity) {
                Err(Fault::new("Unavailable", "service", "expired generation"))
            } else {
                Ok(unit)
            }
        });
        Box::pin(async move {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let worker = tokio::spawn(async move {
                let terminal = async move {
                    let unit = match unit {
                        Ok(unit) => unit,
                        Err(e) => return Terminal::failed(e),
                    };
                    if !unit.open.load(Ordering::Acquire) || cancel.is_cancelled() {
                        return Terminal::failed(Fault::new(
                            "Unavailable",
                            &unit.identity.id,
                            "instance admission closed or call cancelled",
                        ));
                    }
                    let declared = &router.graph.instances[&unit.identity.id].scope;
                    let scope = if router.graph.descendant(&scope, declared) {
                        scope
                    } else if router.graph.descendant(declared, &scope) {
                        declared.clone()
                    } else {
                        return Terminal::failed(Fault::new(
                            "InvalidScope",
                            &unit.identity.id,
                            "instance is bound to an isolated sibling scope",
                        ));
                    };
                    let edge = (unit.identity.id.clone(), request.contract.clone());
                    if stack.contains(&edge) {
                        return Terminal::failed(Fault::new(
                            "RecursiveCall",
                            &unit.identity.id,
                            "use delegate to continue the selected chain",
                        ));
                    }
                    stack.push(edge);
                    let call = router.next.fetch_add(1, Ordering::Relaxed);
                    let identity = CallIdentity {
                        owner: unit.identity.clone(),
                        scope,
                        call,
                        next: (index + 1 < chain.len()).then_some(call),
                        job,
                    };
                    request.execution = Some(identity.clone());
                    router
                        .calls
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(
                            call,
                            LiveCall {
                                identity,
                                request: request.clone(),
                                chain,
                                index,
                                stack,
                                delegated: false,
                            },
                        );
                    router
                        .events
                        .push(request.run_id, "service_called", request.public_trace());
                    let result = unit.instance.call(request, cancel).await;
                    router
                        .calls
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&call);
                    result
                }
                .await;
                let _ = sender.send(terminal);
            });
            {
                let mut workers = self.workers.lock().unwrap_or_else(|e| e.into_inner());
                workers.retain(|task| !task.is_finished());
                workers.push(worker);
            }
            receiver.await.unwrap_or_else(|e| {
                Terminal::failed(Fault::new("PluginFailure", "runtime", e.to_string()))
            })
        })
    }
    fn parent(
        &self,
        owner: &str,
        request: &Request,
    ) -> Result<(CallIdentity, Vec<(String, String)>), Fault> {
        let identity = request
            .execution
            .as_ref()
            .ok_or_else(|| Fault::new("ExpiredCall", owner, "missing invocation identity"))?;
        let calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
        let call = calls
            .get(&identity.call)
            .filter(|c| {
                c.identity.owner.id == owner
                    && c.request.run_id == request.run_id
                    && c.request.session_id == request.session_id
            })
            .ok_or_else(|| Fault::new("ExpiredCall", owner, "invocation has settled"))?;
        Ok((call.identity.clone(), call.stack.clone()))
    }
    pub async fn from_author(
        self: &Arc<Self>,
        owner: String,
        request: Request,
        cancel: Cancellation,
    ) -> Terminal {
        let result = self.author_call(&owner, request, cancel).await;
        match result {
            Ok(value) => Terminal {
                outcome: p::Outcome::Completed(value),
                cleanup_errors: vec![],
                partial_result: None,
            },
            Err(e) => Terminal::failed(e),
        }
    }
    async fn author_call(
        self: &Arc<Self>,
        owner: &str,
        mut request: Request,
        cancel: Cancellation,
    ) -> Result<serde_json::Value, Fault> {
        let (identity, stack) = self.parent(owner, &request)?;
        if request.contract != p::runtime::HOST {
            return self
                .route(request, identity.scope, cancel, stack, identity.job)
                .await
                .into_result();
        }
        let command: HostRequest = serde_json::from_value(request.payload.clone())
            .map_err(|e| Fault::new("InvalidInput", "runtime", e.to_string()))?;
        match command {
            HostRequest::Environment => serde_json::to_value(&self.environment).map_err(codec),
            HostRequest::Call {
                scope,
                contract,
                input,
            } => {
                if !self.graph.descendant(&scope, &identity.scope) {
                    return Err(Fault::new(
                        "InvalidScope",
                        owner,
                        "explicit call must use this scope or a descendant",
                    ));
                }
                request.contract = contract;
                request.payload = input;
                self.route(request, scope, cancel, stack, identity.job)
                    .await
                    .into_result()
            }
            HostRequest::Delegate { token, input } => {
                let (mut downstream, chain, index, scope) = {
                    let mut calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
                    let call = calls
                        .get_mut(&token)
                        .filter(|c| {
                            c.identity.call == identity.call
                                && c.identity.next == Some(token)
                                && !c.delegated
                        })
                        .ok_or_else(|| {
                            Fault::new(
                                "ExpiredContinuation",
                                owner,
                                "continuation expired or consumed",
                            )
                        })?;
                    call.delegated = true;
                    (
                        call.request.clone(),
                        call.chain.clone(),
                        call.index + 1,
                        call.identity.scope.clone(),
                    )
                };
                downstream.payload = input;
                self.dispatch(downstream, scope, chain, index, cancel, stack, identity.job)
                    .await
                    .into_result()
            }
            HostRequest::Submit { contract, input } => {
                let unit = self.unit(owner)?;
                let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
                if !self.open.load(Ordering::Acquire) || !unit.open.load(Ordering::Acquire) {
                    return Err(Fault::new("Unavailable", owner, "job admission closed"));
                }
                if matches!(
                    contract.as_str(),
                    p::INSTANCE_STOP | p::runtime::READY | p::runtime::HOST
                ) || !unit.provides.contains(&contract)
                {
                    return Err(Fault::new(
                        "MissingDependency",
                        owner,
                        "job callback is not a declared ordinary service",
                    ));
                }
                if jobs.values().filter(|j| j.owner == identity.owner).count() >= 1024 {
                    return Err(Fault::new(
                        "ResourceLimit",
                        owner,
                        "forget settled job receipts before submitting more work",
                    ));
                }
                let id = self.next.fetch_add(1, Ordering::Relaxed);
                let job = Arc::new(Job {
                    owner: identity.owner.clone(),
                    cancel: Cancellation::default(),
                    status: tokio::sync::watch::channel(None).0,
                });
                jobs.insert(id, job.clone());
                let router = self.clone();
                let callback = Request {
                    execution: None,
                    session_id: self.session_id,
                    run_id: 0,
                    contract,
                    payload: input,
                };
                let worker_job = job.clone();
                let worker = self.runtime.spawn(async move {
                    let terminal = router
                        .dispatch(
                            callback,
                            identity.scope,
                            vec![identity.owner.id],
                            0,
                            worker_job.cancel.clone(),
                            vec![],
                            Some(id),
                        )
                        .await;
                    worker_job.status.send_replace(Some(terminal));
                    router.events.push(
                        0,
                        "job_settled",
                        serde_json::json!({ "job": id, "owner": worker_job.owner }),
                    );
                });
                let mut workers = self.workers.lock().unwrap_or_else(|e| e.into_inner());
                workers.retain(|task| !task.is_finished());
                workers.push(worker);
                serde_json::to_value(JobStatus {
                    id,
                    owner: job.owner.clone(),
                    terminal: None,
                })
                .map_err(codec)
            }
            HostRequest::Forget { job } => {
                let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
                let entry = jobs
                    .get(&job)
                    .filter(|j| j.owner == identity.owner)
                    .ok_or_else(|| Fault::new("MissingJob", owner, "unknown job"))?;
                if entry.status.borrow().is_none() {
                    return Err(Fault::new(
                        "JobRunning",
                        owner,
                        "join before forgetting a job",
                    ));
                }
                jobs.remove(&job);
                Ok(serde_json::Value::Null)
            }
            HostRequest::Inspect { job }
            | HostRequest::Join { job }
            | HostRequest::Cancel { job } => {
                let entry = self
                    .jobs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&job)
                    .cloned()
                    .filter(|j| j.owner == identity.owner)
                    .ok_or_else(|| {
                        Fault::new(
                            "MissingJob",
                            owner,
                            "job belongs to another owner or is absent",
                        )
                    })?;
                if matches!(command, HostRequest::Cancel { .. }) {
                    entry.cancel.cancel();
                    return Ok(serde_json::Value::Null);
                }
                if matches!(command, HostRequest::Join { .. }) {
                    if identity.job == Some(job) {
                        return Err(Fault::new("InvalidInput", owner, "job cannot join itself"));
                    }
                    let mut status = entry.status.subscribe();
                    loop {
                        if status.borrow_and_update().is_some() {
                            break;
                        }
                        tokio::select! {
                            _ = cancel.cancelled() =>
                                return Err(Fault::new("Cancelled", owner, "join waiter cancelled")),
                            result = status.changed() => {
                                if result.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                }
                let terminal = entry.status.borrow().clone();
                serde_json::to_value(JobStatus {
                    id: job,
                    owner: entry.owner.clone(),
                    terminal,
                })
                .map_err(codec)
            }
            HostRequest::Events { after, kinds } => {
                let events = tokio::select! {
                    _ = cancel.cancelled() =>
                        return Err(Fault::new("Cancelled", owner, "subscription cancelled")),
                    events = self.events.read_after(after) => events?,
                };
                let cursor = events.last().map_or(after, |e| e.sequence);
                let kinds: BTreeSet<_> = kinds.into_iter().collect();
                let events = events
                    .into_iter()
                    .filter(|e| kinds.is_empty() || kinds.contains(&e.kind))
                    .collect();
                serde_json::to_value(EventBatch { cursor, events }).map_err(codec)
            }
        }
    }
    pub fn accept_event(&self, owner: &str, request: Request) {
        if self.parent(owner, &request).is_ok() {
            self.events
                .push(request.run_id, &request.contract, request.payload);
        }
    }
}
fn codec(e: serde_json::Error) -> Fault {
    Fault::new("InvalidInput", "runtime", e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn abandoned_waiter_still_revokes_host_identity_after_cleanup() {
        let (entered, observed) = tokio::sync::oneshot::channel();
        let entered = Arc::new(Mutex::new(Some(entered)));
        let (release, barrier) = tokio::sync::oneshot::channel();
        let barrier = Arc::new(Mutex::new(Some(barrier)));
        let mut package = eden_plugin_sdk::Package::new("fixture");
        for contract in [p::AGENT_LOOP, p::CONTEXT, p::PROVIDER, p::TOOL] {
            let entered = entered.clone();
            let barrier = barrier.clone();
            package = package.service(contract, move |_: (), cx| {
                let entered = entered.clone();
                let barrier = barrier.clone();
                async move {
                    let barrier = barrier.lock().unwrap().take().unwrap();
                    cx.scope.cleanup(async move {
                        barrier.await.unwrap();
                        Ok(())
                    })?;
                    entered
                        .lock()
                        .unwrap()
                        .take()
                        .unwrap()
                        .send(cx.identity().unwrap().clone())
                        .unwrap();
                    std::future::pending::<Result<(), Fault>>().await
                }
            });
        }
        let roles: BTreeMap<_, _> = [p::AGENT_LOOP, p::CONTEXT, p::PROVIDER, p::TOOL]
            .into_iter()
            .map(|r| (r, "fixture"))
            .collect();
        let composition = serde_json::from_value(serde_json::json!({
            "roles": roles,
            "packages": [{
                "descriptor": package.descriptor(),
                "host": p::CONTRACT,
                "sdk": p::CONTRACT,
                "target": TARGET,
                "library": "embedded",
            }],
        }))
        .unwrap();
        let kernel = Arc::new(
            Kernel::load_embedded(
                composition,
                Path::new("."),
                1,
                Events::new(1),
                BTreeMap::from([("fixture".into(), package)]),
            )
            .await
            .unwrap(),
        );
        let caller = kernel.clone();
        let waiter = tokio::spawn(async move {
            caller
                .invoke(
                    Request {
                        execution: None,
                        session_id: 1,
                        run_id: 1,
                        contract: p::CONTEXT.into(),
                        payload: serde_json::Value::Null,
                    },
                    Cancellation::default(),
                )
                .await
        });
        let identity = observed.await.unwrap();
        waiter.abort();
        let _ = waiter.await;
        let stopping = kernel.clone();
        let stop = tokio::spawn(async move { stopping.shutdown().await });
        release.send(()).unwrap();
        stop.await.unwrap().unwrap();
        assert!(kernel.router.calls.lock().unwrap().is_empty());
        let stale = kernel
            .router
            .from_author(
                "fixture".into(),
                Request {
                    execution: Some(identity),
                    session_id: 1,
                    run_id: 1,
                    contract: p::CONTEXT.into(),
                    payload: serde_json::Value::Null,
                },
                Cancellation::default(),
            )
            .await;
        assert_eq!(stale.into_result().unwrap_err().code, "ExpiredCall");
    }
}
