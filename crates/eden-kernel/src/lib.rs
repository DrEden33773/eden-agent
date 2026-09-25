//! Composition preflight, native loading, explicit role selection, the event
//! ledger, and host-owned lifecycle.
mod graph;
pub mod history;
mod native;
mod routing;
use cordis_core::{Context, FiberHandle, Plugin, PreparedPlugin, Service};
use eden_plugin_sdk::{
    Cancellation,
    abi::{Bytes, HostApi, Reply, TARGET},
};
use eden_protocol::{self as p, Composition, Event, Fault, Request, Terminal};
pub use native::NativeInstance;
use routing::Router;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

/// Validate all composition metadata before executing any library code.
pub fn preflight(composition: &Composition) -> Result<(), Fault> {
    if composition.roles.contains_key(p::INSTANCE_STOP) {
        return Err(Fault::new(
            "InvalidInput",
            "manifest",
            "instance finalization cannot be selected as a public service",
        ));
    }

    let mut packages = BTreeMap::new();
    for package in &composition.packages {
        if package.host != p::CONTRACT || package.sdk != p::CONTRACT || package.target != TARGET {
            return Err(Fault::new(
                "IncompatibleContract",
                "manifest",
                "host, SDK, or target mismatch",
            ));
        }
        let descriptor = &package.descriptor;
        if descriptor.package.is_empty()
            || descriptor.version.is_empty()
            || package.library.is_empty()
            || packages.insert(&descriptor.package, descriptor).is_some()
        {
            return Err(Fault::new(
                "InvalidInput",
                "manifest",
                "empty or duplicate package identity",
            ));
        }
        let mut roles = std::collections::BTreeSet::new();
        if descriptor
            .provides
            .iter()
            .any(|role| role.is_empty() || !roles.insert(role))
        {
            return Err(Fault::new(
                "InvalidInput",
                "manifest",
                "empty or duplicate role",
            ));
        }
    }
    let required: &[&str] = if composition.roles.contains_key(p::coding::LOOP) {
        &[
            p::coding::LOOP,
            p::coding::CONTEXT,
            p::coding::PROVIDER,
            p::coding::TOOL,
            p::coding::STORE,
            p::coding::QUEUE,
        ]
    } else if !composition.roles.contains_key(p::AGENT_LOOP)
        && !composition.roles.is_empty()
        && composition.roles.keys().all(|role| {
            matches!(
                role.as_str(),
                p::delivery::EXPORTER | p::delivery::SHARE_TARGET | p::updates::UPDATE_SOURCE
            )
        })
    {
        &[]
    } else {
        &[p::AGENT_LOOP, p::CONTEXT, p::PROVIDER, p::TOOL]
    };
    for role in required.iter().copied() {
        if !composition.roles.contains_key(role) {
            return Err(Fault::new("MissingDependency", "composition", role));
        }
    }
    graph::Graph::build(composition)?;
    Ok(())
}
/// In-memory ordered event ledger, shared by native routes and session callers.
pub struct Events {
    session_id: u64,
    ledger: Mutex<EventLedger>,
    closed: AtomicBool,
    changed: tokio::sync::Notify,
}
struct EventLedger {
    entries: std::collections::VecDeque<Event>,
    next: u64,
    bytes: usize,
    attempts: BTreeMap<u64, Vec<Event>>,
}
impl Events {
    /// Retain up to 8192 events and approximately 8 MiB of serialized payloads.
    /// Older cursors report lag explicitly; public history remains the durable source.
    pub fn new(session_id: u64) -> Arc<Self> {
        Arc::new(Self {
            session_id,
            ledger: Mutex::new(EventLedger {
                entries: Default::default(),
                next: 1,
                bytes: 0,
                attempts: BTreeMap::new(),
            }),
            closed: AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
        })
    }
    /// Append an observation using a session-local monotonically increasing sequence.
    pub fn push(&self, run_id: u64, kind: &str, payload: serde_json::Value) {
        let mut ledger = self.ledger.lock().unwrap_or_else(|e| e.into_inner());
        let event = Event {
            sequence: ledger.next,
            session_id: self.session_id,
            run_id,
            kind: kind.into(),
            payload,
        };
        ledger.next += 1;
        // Persistence needs interrupted text even after an observer falls behind. Coalescing
        // text deltas keeps one text value per attempt rather than retaining every delta event.
        if matches!(
            kind,
            "model_request"
                | "model_attempt_started"
                | "model_attempt_finished"
                | "model_text_delta"
        ) {
            let attempts = ledger.attempts.entry(run_id).or_default();
            if kind == "model_text_delta" {
                let previous = attempts.iter_mut().rev().find(|e| {
                    e.kind == kind && e.payload["attempt_id"] == event.payload["attempt_id"]
                });
                if let Some(previous) = previous {
                    if let (Some(text), Some(delta)) = (
                        previous.payload["delta"].as_str(),
                        event.payload["delta"].as_str(),
                    ) {
                        previous.payload["delta"] =
                            serde_json::Value::String(format!("{text}{delta}"));
                    }
                } else {
                    attempts.push(event.clone());
                }
            } else {
                attempts.push(event.clone());
            }
        }
        ledger.bytes += event_size(&event);
        ledger.entries.push_back(event);
        while ledger.entries.len() > 8192 || ledger.bytes > 8 * 1024 * 1024 {
            if let Some(old) = ledger.entries.pop_front() {
                ledger.bytes = ledger.bytes.saturating_sub(event_size(&old));
            } else {
                break;
            }
        }
        self.changed.notify_waiters();
    }
    /// Drain compacted attempt observations after execution settles, independently of cursors.
    pub fn take_attempts(&self, run_id: u64) -> Vec<Event> {
        self.ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .attempts
            .remove(&run_id)
            .unwrap_or_default()
    }
    /// The retained observation window; it is not the durable session history.
    pub fn snapshot(&self) -> Vec<Event> {
        self.ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .iter()
            .cloned()
            .collect()
    }
    /// End observation after all producers settle and wake idle readers.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }
    /// Return retained events after the cursor, or `Lagged` if any have expired.
    /// Empty means the owner closed the ledger and this cursor has drained it.
    pub async fn read_after(&self, sequence: u64) -> Result<Vec<Event>, Fault> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let ledger = self.ledger.lock().unwrap_or_else(|e| e.into_inner());
                let oldest = ledger.entries.front().map_or(ledger.next, |e| e.sequence);
                if sequence.saturating_add(1) < oldest {
                    return Err(Fault::new(
                        "Lagged",
                        "events",
                        format!("cursor {sequence} expired; first retained sequence is {oldest}"),
                    ));
                }
                let events: Vec<_> = ledger
                    .entries
                    .iter()
                    .filter(|e| e.sequence > sequence)
                    .cloned()
                    .collect();
                if !events.is_empty() || self.closed.load(Ordering::Acquire) {
                    return Ok(events);
                }
            }
            changed.await;
        }
    }
    /// Compatibility observer: lag is an explicit `events_lagged` observation. New clients
    /// should use `read_after` to distinguish lag from an ordinary domain event.
    pub async fn after(&self, sequence: u64) -> Vec<Event> {
        match self.read_after(sequence).await {
            Ok(events) => events,
            Err(error) => {
                let ledger = self.ledger.lock().unwrap_or_else(|e| e.into_inner());
                vec![Event {
                    sequence: ledger
                        .entries
                        .front()
                        .map_or(ledger.next, |e| e.sequence)
                        .saturating_sub(1),
                    session_id: self.session_id,
                    run_id: 0,
                    kind: "events_lagged".into(),
                    payload: serde_json::json!(error),
                }]
            }
        }
    }
}
fn event_size(event: &Event) -> usize {
    event.kind.len() + event.payload.to_string().len() + 40
}
enum Instance {
    Native(Arc<NativeInstance>),
    Local(Arc<eden_plugin_sdk::local::LocalInstance>),
}
impl Instance {
    fn close(&self) {
        match self {
            Self::Native(i) => i.close(),
            Self::Local(i) => i.close(),
        }
    }
    async fn stop(&self) -> Result<(), Fault> {
        match self {
            Self::Native(i) => i.stop().await,
            Self::Local(i) => i.stop().await,
        }
    }
    async fn finalize(&self, request: Request) -> Result<(), Fault> {
        match self {
            Self::Native(i) => i.stop_with_request(request).await,
            Self::Local(i) => i.stop_with_request(request).await,
        }
    }
    async fn call(&self, request: Request, cancel: Cancellation) -> Terminal {
        match self {
            Self::Native(i) => i.call(request, cancel).await,
            Self::Local(i) => i.call(request, cancel).await,
        }
    }
}
struct Publication(Arc<Instance>);
impl Service for Publication {
    const NAME: &'static str = "eden.instance-publication.v1";
}
struct Adapter;
impl Plugin for Adapter {
    type Config = Arc<Publication>;
    type Input = Arc<Publication>;
    type PrepareError = std::convert::Infallible;
    type ApplyError = std::io::Error;
    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }
    async fn apply(
        &self,
        context: Context,
        publication: &Self::Input,
    ) -> Result<(), Self::ApplyError> {
        let cleanup = publication.clone();
        context
            .effect(move || async move {
                let _ = cleanup.0.stop().await;
            })
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let _publication = context
            .provide(publication.clone())
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(())
    }
}
struct Managed {
    identity: p::runtime::InstanceIdentity,
    instance: Arc<Instance>,
    provides: Vec<String>,
    token: usize,
    open: AtomicBool,
    fiber: FiberHandle,
    _context: Context,
    stop_lock: tokio::sync::Mutex<()>,
}
// HostApi tokens use a process registry, so retained author contexts cannot dereference freed hosts.
type RouterRegistry = Mutex<BTreeMap<usize, (std::sync::Weak<Router>, String)>>;
static ROUTERS: std::sync::OnceLock<RouterRegistry> = std::sync::OnceLock::new();
static NEXT_ROUTER: AtomicU64 = AtomicU64::new(1);
fn routers() -> &'static RouterRegistry {
    ROUTERS.get_or_init(Mutex::default)
}
fn router(id: usize) -> Option<Arc<Router>> {
    routers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .and_then(|(router, _)| router.upgrade())
}
fn origin(id: usize) -> Option<String> {
    routers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .map(|(_, owner)| owner.clone())
}
unsafe extern "C" fn request(context: usize, bytes: Bytes, reply: Reply) -> u64 {
    let Some(router) = router(context) else {
        return 0;
    };
    // SAFETY: SDK request bytes are kept live during the call and copied by decode.
    let input = unsafe { bytes.decode::<Request>() }.map(|mut request| {
        if request.contract == eden_protocol::presentation::HOST
            && let Some(payload) = request.payload.as_object_mut()
        {
            payload.insert(
                "owner".into(),
                serde_json::Value::String(origin(context).unwrap_or_default()),
            );
        }
        request
    });
    let id = router.next.fetch_add(1, Ordering::Relaxed);
    let cancel = Cancellation::default();
    router
        .requests
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, cancel.clone());
    let worker_router = router.clone();
    let worker = router.runtime.spawn(async move {
        let terminal = match input {
            Ok(request) => {
                worker_router
                    .from_author(origin(context).unwrap_or_default(), request, cancel)
                    .await
            }
            Err(error) => Terminal::failed(error),
        };
        // SAFETY: An accepted request owns this reply token until its single completion.
        unsafe {
            reply.send(&terminal);
        }
        worker_router
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    });
    let mut workers = router.workers.lock().unwrap_or_else(|e| e.into_inner());
    workers.retain(|task| !task.is_finished());
    workers.push(worker);
    id
}
unsafe extern "C" fn cancel(context: usize, id: u64) {
    if let Some(router) = router(context)
        && let Some(cancel) = router
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
    {
        cancel.cancel();
    }
}
unsafe extern "C" fn event(context: usize, bytes: Bytes) {
    let Some(router) = router(context) else {
        return;
    };
    // SAFETY: SDK owns the span throughout this synchronous callback.
    if let Ok(event) = unsafe { bytes.decode::<Request>() }
        && event.session_id == router.session_id
    {
        router.accept_event(&origin(context).unwrap_or_default(), event);
    }
}
/// A composition of independently disposable native instance publications.
pub struct Kernel {
    router: Arc<Router>,
    composition: Composition,
}
async fn rollback_initialization(mut error: Fault, instances: &[Arc<Instance>]) -> Fault {
    for instance in instances {
        instance.close();
    }
    for instance in instances {
        if let Err(cleanup) = instance.stop().await {
            error
                .message
                .push_str(&format!("; rollback cleanup: {cleanup}"));
        }
    }
    error
}
impl Kernel {
    /// Resolve relative native paths against the composition file, never the caller's cwd.
    pub async fn load(path: &Path, session_id: u64, events: Arc<Events>) -> Result<Self, Fault> {
        let bytes = std::fs::read(path)
            .map_err(|e| Fault::new("InvalidInput", "composition", e.to_string()))?;
        let composition = serde_json::from_slice(&bytes)
            .map_err(|e| Fault::new("InvalidInput", "composition", e.to_string()))?;
        Self::load_resolved(
            composition,
            path.parent().unwrap_or(Path::new(".")),
            session_id,
            events,
        )
        .await
    }
    /// Mount only explicitly resolved and authorized native packages.
    pub async fn load_resolved(
        composition: Composition,
        base: &Path,
        session_id: u64,
        events: Arc<Events>,
    ) -> Result<Self, Fault> {
        Self::load_embedded(composition, base, session_id, events, BTreeMap::new()).await
    }
    /// Caller-owned packages use the same routing and lifecycle contract as native authors.
    /// Each local contribution supplies one already-created instance.
    pub async fn load_embedded(
        composition: Composition,
        base: &Path,
        session_id: u64,
        events: Arc<Events>,
        local: BTreeMap<String, eden_plugin_sdk::Package>,
    ) -> Result<Self, Fault> {
        let base = base.to_owned();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = Self::load_owned(composition, &base, session_id, events, local).await;
            let _ = sender.send(Delivery(Some(result)));
        });
        let mut delivery = receiver
            .await
            .map_err(|e| Fault::new("Unavailable", "initialize", e.to_string()))?;
        delivery
            .0
            .take()
            .ok_or_else(|| Fault::new("Unavailable", "initialize", "missing delivery"))?
    }
    async fn load_owned(
        composition: Composition,
        base: &Path,
        session_id: u64,
        events: Arc<Events>,
        mut local: BTreeMap<String, eden_plugin_sdk::Package>,
    ) -> Result<Self, Fault> {
        preflight(&composition)?;
        let graph = graph::Graph::build(&composition)?;
        for (name, package) in &local {
            if !composition
                .packages
                .iter()
                .any(|m| m.descriptor.package == *name && &m.descriptor == package.descriptor())
                || graph
                    .instances
                    .values()
                    .filter(|i| &i.package == name)
                    .count()
                    != 1
            {
                return Err(Fault::new(
                    "IncompatibleContract",
                    "embedded",
                    "local descriptor differs or contribution has multiple instances",
                ));
            }
        }
        let mut manifests = BTreeMap::new();
        for manifest in &composition.packages {
            let path = if local.contains_key(&manifest.descriptor.package) {
                std::path::PathBuf::new()
            } else {
                std::fs::canonicalize(base.join(&manifest.library)).map_err(|e| {
                    Fault::new(
                        "MissingDependency",
                        &manifest.descriptor.package,
                        format!(
                            "cannot resolve native library {}: {e}; restore the plugin file or \
                             select a composition with installed libraries",
                            base.join(&manifest.library).display()
                        ),
                    )
                })?
            };
            manifests.insert(
                manifest.descriptor.package.clone(),
                (manifest.clone(), path),
            );
        }
        let router = Arc::new(Router {
            session_id,
            events,
            open: AtomicBool::new(true),
            next: AtomicU64::new(1),
            requests: Mutex::new(BTreeMap::new()),
            workers: Mutex::new(vec![]),
            runtime: tokio::runtime::Handle::current(),
            graph,
            environment: composition.host_environment.clone(),
            instances: Mutex::new(BTreeMap::new()),
            calls: Mutex::new(BTreeMap::new()),
            jobs: Mutex::new(BTreeMap::new()),
        });
        let kernel = Self {
            router,
            composition,
        };
        let result = kernel.initialize(&manifests, &mut local).await;
        if let Err(mut error) = result {
            if let Err(cleanup) = kernel.shutdown().await {
                error
                    .message
                    .push_str(&format!("; rollback cleanup: {cleanup}"));
            }
            return Err(error);
        }
        Ok(kernel)
    }
    async fn initialize(
        &self,
        manifests: &BTreeMap<String, (p::PackageManifest, std::path::PathBuf)>,
        local: &mut BTreeMap<String, eden_plugin_sdk::Package>,
    ) -> Result<(), Fault> {
        let host_environment = self
            .router
            .environment
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| Fault::new("InvalidInput", "host-environment", e.to_string()))?;
        for id in &self.router.graph.order {
            let spec = &self.router.graph.instances[id];
            let (mut manifest, path) = manifests[&spec.package].clone();
            // Instance overrides cannot replace authority, including scalar-to-object configs.
            let environment = host_environment
                .clone()
                .or_else(|| manifest.config.get(p::environment::CONFIG_KEY).cloned());
            if let Some(config) = &spec.config {
                manifest.config = config.clone();
            }
            if let Some(environment) = environment {
                if manifest.config.is_null() {
                    manifest.config = serde_json::json!({});
                }
                if manifest.config.is_object() {
                    manifest.config[p::environment::CONFIG_KEY] = environment;
                }
            }
            let token = NEXT_ROUTER.fetch_add(1, Ordering::Relaxed) as usize;
            routers()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(token, (Arc::downgrade(&self.router), id.clone()));
            let host = HostApi {
                context: token,
                request,
                cancel,
                event,
            };
            let loaded = if let Some(package) = local.remove(&spec.package) {
                // SAFETY: Registry callbacks remain callable until the instance disposal barrier.
                Ok(Arc::new(Instance::Local(unsafe {
                    eden_plugin_sdk::local::LocalInstance::new(package, host)
                })))
            } else {
                let input = manifest.clone();
                tokio::task::spawn_blocking(move || NativeInstance::load(&path, &input, host))
                    .await
                    .map_err(|e| Fault::new("Unavailable", "initialize", e.to_string()))
                    .and_then(|r| r)
                    .map(|i| Arc::new(Instance::Native(i)))
            };
            let instance = match loaded {
                Ok(i) => i,
                Err(e) => {
                    routers()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&token);
                    return Err(e);
                }
            };
            let context = Context::new();
            let fiber = match context
                .spawn(PreparedPlugin::from_input(
                    Adapter,
                    Arc::new(Publication(instance.clone())),
                ))
                .await
            {
                Ok(fiber) => fiber,
                Err(e) => {
                    let error = rollback_initialization(
                        Fault::new("Unavailable", "cordis", e.to_string()),
                        &[instance],
                    )
                    .await;
                    routers()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&token);
                    return Err(error);
                }
            };
            self.router
                .instances
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    id.clone(),
                    Arc::new(Managed {
                        identity: p::runtime::InstanceIdentity {
                            id: id.clone(),
                            generation: token as u64,
                        },
                        instance,
                        provides: manifest.descriptor.provides,
                        token,
                        open: AtomicBool::new(true),
                        fiber,
                        _context: context,
                        stop_lock: tokio::sync::Mutex::new(()),
                    }),
                );
        }
        for id in &self.router.graph.order {
            let unit = self.router.unit(id)?;
            if unit.provides.iter().any(|c| c == p::runtime::READY) {
                self.router
                    .dispatch(
                        Request {
                            execution: None,
                            session_id: self.router.session_id,
                            run_id: 0,
                            contract: p::runtime::READY.into(),
                            payload: serde_json::Value::Null,
                        },
                        self.router.graph.instances[id].scope.clone(),
                        vec![id.clone()],
                        0,
                        Cancellation::default(),
                        vec![],
                        None,
                    )
                    .await
                    .into_result()?;
            }
        }
        Ok(())
    }
    /// The mounted declaration, including stable instance and scope bindings.
    pub fn composition(&self) -> &Composition {
        &self.composition
    }
    /// Calls settle only after their operation-owned cleanup and native bridges finish.
    pub async fn invoke(&self, request: Request, cancel: Cancellation) -> Terminal {
        self.router.invoke(request, cancel).await
    }
    /// Invoke a declared service on one stable instance, bypassing role selection for owner actions.
    pub async fn invoke_package(
        &self,
        instance: &str,
        request: Request,
        cancel: Cancellation,
    ) -> Terminal {
        if request.session_id != self.router.session_id || !self.router.open.load(Ordering::Acquire)
        {
            return Terminal::failed(Fault::new(
                "SessionMismatch",
                "router",
                "foreign or closed session",
            ));
        }
        let unit = match self.router.unit(instance) {
            Ok(i) => i,
            Err(e) => return Terminal::failed(e),
        };
        if !unit.provides.contains(&request.contract)
            || matches!(
                request.contract.as_str(),
                p::INSTANCE_STOP | p::runtime::READY
            )
        {
            return Terminal::failed(Fault::new(
                "MissingDependency",
                "router",
                "owner does not provide public contract",
            ));
        }
        self.router
            .dispatch(
                request,
                self.router.graph.instances[instance].scope.clone(),
                vec![instance.into()],
                0,
                cancel,
                vec![],
                None,
            )
            .await
    }
    /// Whether the session scope resolves this public contract.
    pub fn has_role(&self, contract: &str) -> bool {
        self.router.graph.binding("", contract).is_ok()
    }
    /// Retain the selected chain and its incarnation gates. Existing low-level callers use
    /// the same wrappers, ownership and service access as ordinary session calls.
    pub fn role(&self, contract: &str) -> Result<Arc<ServiceHandle>, Fault> {
        let binding = self.router.graph.binding("", contract)?;
        let owners = binding
            .wrappers
            .iter()
            .chain(std::iter::once(&binding.tail))
            .map(|id| self.router.unit(id).map(|u| u.identity.clone()))
            .collect::<Result<_, _>>()?;
        Ok(Arc::new(ServiceHandle {
            router: Arc::downgrade(&self.router),
            contract: contract.into(),
            owners,
        }))
    }
    /// Return the incarnation used by stale-handle and future configuration management checks.
    pub fn instance_identity(&self, id: &str) -> Result<p::runtime::InstanceIdentity, Fault> {
        Ok(self.router.unit(id)?.identity.clone())
    }
    /// Close one instance's admission immediately. Dependency cleanup may still call live providers.
    pub fn quiesce_instance(&self, id: &str) -> Result<(), Fault> {
        let unit = self.router.unit(id)?;
        let jobs = self.router.jobs.lock().unwrap_or_else(|e| e.into_inner());
        unit.open.store(false, Ordering::Release);
        for job in jobs.values().filter(|j| j.owner == unit.identity) {
            job.cancel.cancel();
        }
        unit.instance.close();
        Ok(())
    }
    /// Await one independently owned publication's jobs, bridges and finalizer. Dropping the waiter
    /// cannot abandon shutdown. Callers manage dependency impact before invoking this primitive.
    pub async fn stop_instance(&self, id: &str) -> Result<(), Fault> {
        self.quiesce_instance(id)?;
        let router = self.router.clone();
        let id = id.to_owned();
        tokio::spawn(async move { stop_unit(router, id).await })
            .await
            .map_err(|e| Fault::new("CleanupFailure", "runtime", e.to_string()))?
    }
    /// Dispose dependents before their declared owners/providers, retaining cleanup service access.
    pub async fn shutdown(&self) -> Result<(), Fault> {
        self.router.open.store(false, Ordering::Release);
        let router = self.router.clone();
        tokio::spawn(async move {
            let mut failure: Option<Fault> = None;
            for id in router.graph.order.iter().rev() {
                if router.unit(id).is_err() {
                    continue;
                }
                if let Err(error) = stop_unit(router.clone(), id.clone()).await {
                    if let Some(first) = &mut failure {
                        first
                            .message
                            .push_str(&format!("; instance cleanup: {error}"));
                    } else {
                        failure = Some(error);
                    }
                }
            }
            loop {
                let workers =
                    std::mem::take(&mut *router.workers.lock().unwrap_or_else(|e| e.into_inner()));
                if workers.is_empty() {
                    break;
                }
                for worker in workers {
                    let _ = worker.await;
                }
            }
            failure.map_or(Ok(()), Err)
        })
        .await
        .map_err(|e| Fault::new("CleanupFailure", "runtime", e.to_string()))?
    }
}
/// A selected service handle expires when any member of its captured chain stops.
pub struct ServiceHandle {
    router: std::sync::Weak<Router>,
    contract: String,
    owners: Vec<p::runtime::InstanceIdentity>,
}
impl ServiceHandle {
    /// Invoke the captured chain with cleanup ownership; stale handles never bind to replacements.
    pub async fn call(&self, request: Request, cancel: Cancellation) -> Terminal {
        let Some(router) = self.router.upgrade() else {
            return Terminal::failed(Fault::new("Unavailable", "service", "owner dropped"));
        };
        if request.contract != self.contract
            || self.owners.iter().any(|id| {
                router.unit(&id.id).map_or(true, |u| {
                    u.identity != *id || !u.open.load(Ordering::Acquire)
                })
            })
        {
            return Terminal::failed(Fault::new(
                "Unavailable",
                "service",
                "contract mismatch or expired generation",
            ));
        }
        if request.session_id != router.session_id || !router.open.load(Ordering::Acquire) {
            return Terminal::failed(Fault::new(
                "Unavailable",
                "service",
                "foreign or closed session",
            ));
        }
        router
            .dispatch(
                request,
                String::new(),
                self.owners.iter().map(|id| id.id.clone()).collect(),
                0,
                cancel,
                vec![],
                None,
            )
            .await
    }
}
async fn stop_unit(router: Arc<Router>, id: String) -> Result<(), Fault> {
    let unit = router.unit(&id)?;
    let _stop = unit.stop_lock.lock().await;
    let jobs: Vec<_> = {
        let jobs = router.jobs.lock().unwrap_or_else(|e| e.into_inner());
        unit.open.store(false, Ordering::Release);
        jobs.values()
            .filter(|j| j.owner == unit.identity)
            .cloned()
            .collect()
    };
    for job in &jobs {
        job.cancel.cancel();
    }
    unit.instance.close();
    let mut failure = None;
    for job in jobs {
        let mut status = job.status.subscribe();
        loop {
            if let Some(terminal) = status.borrow_and_update().clone() {
                if let Some(error) = terminal.cleanup_errors.first() {
                    failure = Some(error.clone());
                }
                break;
            }
            if status.changed().await.is_err() {
                break;
            }
        }
    }
    let call = router.next.fetch_add(1, Ordering::Relaxed);
    let identity = p::runtime::CallIdentity {
        owner: unit.identity.clone(),
        scope: router.graph.instances[&id].scope.clone(),
        call,
        next: None,
        job: None,
    };
    let request = Request {
        execution: Some(identity.clone()),
        session_id: router.session_id,
        run_id: 0,
        contract: p::INSTANCE_STOP.into(),
        payload: serde_json::Value::Null,
    };
    router
        .calls
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            call,
            routing::LiveCall {
                identity,
                request: request.clone(),
                chain: vec![id.clone()],
                index: 0,
                stack: vec![(id.clone(), p::INSTANCE_STOP.into())],
                delegated: false,
            },
        );
    let result = unit.instance.finalize(request).await;
    router
        .calls
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&call);
    let disposed = unit
        .fiber
        .dispose()
        .await
        .map_err(|e| Fault::new("CleanupFailure", "cordis", e.to_string()));
    routers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&unit.token);
    result.and(disposed).and(failure.map_or(Ok(()), Err))
}
struct Delivery(Option<Result<Kernel, Fault>>);
impl Drop for Delivery {
    fn drop(&mut self) {
        if let Some(Ok(kernel)) = self.0.take() {
            kernel.router.runtime.clone().spawn(async move {
                let _ = kernel.shutdown().await;
            });
        }
    }
}
