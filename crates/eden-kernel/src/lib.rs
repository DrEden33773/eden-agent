//! Composition preflight, native loading, explicit role selection, the event
//! ledger, and host-owned lifecycle.
pub mod history;
mod native;
use cordis_core::{Context, FiberHandle, Plugin, PreparedPlugin, Service};
use eden_plugin_sdk::{
    Cancellation,
    abi::{Bytes, HostApi, Reply, TARGET},
};
use eden_protocol::{self as p, Composition, Event, Fault, Request, Terminal};
pub use native::NativeInstance;
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
    for (role, package) in &composition.roles {
        if !packages
            .get(package)
            .is_some_and(|descriptor| descriptor.provides.contains(role))
        {
            return Err(Fault::new(
                "MissingDependency",
                "composition",
                format!("{package} does not provide {role}"),
            ));
        }
    }
    for package in &composition.packages {
        for required in &package.requires {
            if !composition.roles.contains_key(required) {
                return Err(Fault::new(
                    "MissingDependency",
                    &package.descriptor.package,
                    format!("required contract is not selected: {required}"),
                ));
            }
        }
    }
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
    async fn call(&self, request: Request, cancel: Cancellation) -> Terminal {
        match self {
            Self::Native(i) => i.call(request, cancel).await,
            Self::Local(i) => i.call(request, cancel).await,
        }
    }
}
struct Services(BTreeMap<String, Arc<Instance>>);
impl Service for Services {
    const NAME: &'static str = "eden.composition-services.v1";
}
struct Adapter;
impl Plugin for Adapter {
    type Config = Arc<Services>;
    type Input = Arc<Services>;
    type PrepareError = std::convert::Infallible;
    type ApplyError = std::io::Error;
    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }
    async fn apply(
        &self,
        context: Context,
        services: &Self::Input,
    ) -> Result<(), Self::ApplyError> {
        let cleanup = services.clone();
        context
            .effect(move || async move {
                for instance in cleanup.0.values() {
                    instance.close();
                }
                for instance in cleanup.0.values() {
                    let _ = instance.stop().await;
                }
            })
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let _publication = context
            .provide(services.clone())
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(())
    }
}
struct Router {
    context: Context,
    session_id: u64,
    events: Arc<Events>,
    open: AtomicBool,
    next: AtomicU64,
    requests: Mutex<BTreeMap<u64, Cancellation>>,
    workers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    runtime: tokio::runtime::Handle,
}
impl Router {
    async fn invoke(&self, request: Request, cancel: Cancellation) -> Terminal {
        if request.session_id != self.session_id {
            return Terminal::failed(Fault::new("InvalidInput", "router", "foreign session"));
        }
        let services = match self.context.try_service::<Services>() {
            Ok(services) => services,
            Err(e) => {
                return Terminal::failed(Fault::new("Unavailable", "services", e.to_string()));
            }
        };
        let Some(instance) = services.0.get(&request.contract) else {
            return Terminal::failed(Fault::new("MissingDependency", "router", &request.contract));
        };
        self.events
            .push(request.run_id, "service_called", request.public_trace());
        instance.call(request, cancel).await
    }
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
    if !router.open.load(Ordering::Acquire) {
        return 0;
    }
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
            Ok(request) => worker_router.invoke(request, cancel).await,
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
    router
        .workers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(worker);
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
        router
            .events
            .push(event.run_id, &event.contract, event.payload);
    }
}
/// One explicitly mounted native composition and its Cordis disposal barrier.
pub struct Kernel {
    router: Arc<Router>,
    tokens: Vec<usize>,
    fiber: FiberHandle,
    instances: Vec<Arc<Instance>>,
    packages: BTreeMap<String, Arc<Instance>>,
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
    /// Read and parse a composition file, then mount it with its own directory
    /// as the base for relative library paths.
    pub async fn load(path: &Path, session_id: u64, events: Arc<Events>) -> Result<Self, Fault> {
        let bytes = std::fs::read(path)
            .map_err(|e| Fault::new("InvalidInput", "composition", e.to_string()))?;
        let composition: Composition = serde_json::from_slice(&bytes)
            .map_err(|e| Fault::new("InvalidInput", "composition", e.to_string()))?;
        Self::load_resolved(
            composition,
            path.parent().unwrap_or(Path::new(".")),
            session_id,
            events,
        )
        .await
    }
    /// Load an explicitly resolved and authorized composition; this never discovers or installs code.
    pub async fn load_resolved(
        composition: Composition,
        base: &Path,
        session_id: u64,
        events: Arc<Events>,
    ) -> Result<Self, Fault> {
        Self::load_embedded(composition, base, session_id, events, BTreeMap::new()).await
    }
    /// Mount explicit caller-owned packages alongside native libraries, with the same role routing.
    /// A local package must exactly match its manifest descriptor; its library field is an identity
    /// marker and is never opened. No Rust value crosses a dynamic-library boundary.
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
            // Delivery's Drop rolls back if the caller leaves at any point before taking ownership.
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
        for (name, package) in &local {
            if !composition.packages.iter().any(|manifest| {
                manifest.descriptor.package == *name && &manifest.descriptor == package.descriptor()
            }) {
                return Err(Fault::new(
                    "IncompatibleContract",
                    "embedded",
                    "local package descriptor differs from manifest",
                ));
            }
        }
        // Resolve all paths before executing the first native library.
        let paths: Vec<_> = composition
            .packages
            .iter()
            .map(|p| {
                if local.contains_key(&p.descriptor.package) {
                    return Ok(std::path::PathBuf::new());
                }
                std::fs::canonicalize(base.join(&p.library)).map_err(|e| {
                    Fault::new(
                        "MissingDependency",
                        &p.descriptor.package,
                        format!(
                            "cannot resolve native library {}: {e}; restore the plugin file or \
                             select a composition with installed libraries",
                            base.join(&p.library).display()
                        ),
                    )
                })
            })
            .collect::<Result<_, _>>()?;
        let context = Context::new();
        let router = Arc::new(Router {
            context: context.clone(),
            session_id,
            events,
            open: AtomicBool::new(true),
            next: AtomicU64::new(1),
            requests: Mutex::new(BTreeMap::new()),
            workers: Mutex::new(vec![]),
            runtime: tokio::runtime::Handle::current(),
        });
        let mut tokens = Vec::new();
        let mut packages: BTreeMap<String, Arc<Instance>> = BTreeMap::new();
        for (manifest, path) in composition.packages.iter().zip(paths) {
            let manifest = manifest.clone();
            let name = manifest.descriptor.package.clone();
            let token = NEXT_ROUTER.fetch_add(1, Ordering::Relaxed) as usize;
            routers()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(token, (Arc::downgrade(&router), name.clone()));
            tokens.push(token);
            let host = HostApi {
                context: token,
                request,
                cancel,
                event,
            };
            if let Some(package) = local.remove(&name) {
                // SAFETY: Router registry callbacks remain live until every instance stops.
                let instance = unsafe { eden_plugin_sdk::local::LocalInstance::new(package, host) };
                packages.insert(name, Arc::new(Instance::Local(instance)));
                continue;
            }
            let result =
                tokio::task::spawn_blocking(move || NativeInstance::load(&path, &manifest, host))
                    .await
                    .map_err(|e| Fault::new("Unavailable", "initialize", e.to_string()))
                    .and_then(|r| r);
            match result {
                Ok(instance) => {
                    packages.insert(name, Arc::new(Instance::Native(instance)));
                }
                Err(error) => {
                    router.open.store(false, Ordering::Release);
                    let instances: Vec<_> = packages.values().cloned().collect();
                    let error = rollback_initialization(error, &instances).await;
                    let mut registry = routers().lock().unwrap_or_else(|e| e.into_inner());
                    for token in &tokens {
                        registry.remove(token);
                    }
                    return Err(error);
                }
            }
        }
        let instances: Vec<_> = packages.values().cloned().collect();
        let package_instances = packages.clone();
        let roles = composition
            .roles
            .clone()
            .into_iter()
            .filter_map(|(role, name)| packages.get(&name).map(|instance| (role, instance.clone())))
            .collect();
        let fiber = match context
            .spawn(PreparedPlugin::from_input(
                Adapter,
                Arc::new(Services(roles)),
            ))
            .await
        {
            Ok(fiber) => fiber,
            Err(error) => {
                router.open.store(false, Ordering::Release);
                let error = rollback_initialization(
                    Fault::new("Unavailable", "cordis", error.to_string()),
                    &instances,
                )
                .await;
                let mut registry = routers().lock().unwrap_or_else(|e| e.into_inner());
                for token in &tokens {
                    registry.remove(token);
                }
                return Err(error);
            }
        };
        Ok(Self {
            router,
            tokens,
            fiber,
            instances,
            packages: package_instances,
            composition,
        })
    }
    /// The composition this kernel mounted, including its role bindings.
    pub fn composition(&self) -> &Composition {
        &self.composition
    }
    /// Route one request to the roles this composition selected, and settle it
    /// with its cleanup. Cancellation is the caller's decision, passed in.
    pub async fn invoke(&self, request: Request, cancel: Cancellation) -> Terminal {
        self.router.invoke(request, cancel).await
    }
    /// Invoke a declared service on its owning package. Presentation actions use this
    /// route so two authors can expose the same action contract without selecting one
    /// global role winner. The package and role must both be installed in this generation.
    pub async fn invoke_package(
        &self,
        package: &str,
        request: Request,
        cancel: Cancellation,
    ) -> Terminal {
        if request.session_id != self.router.session_id {
            return Terminal::failed(Fault::new("SessionMismatch", "router", "foreign session"));
        }
        let declared = self.composition.packages.iter().any(|manifest| {
            manifest.descriptor.package == package
                && manifest
                    .descriptor
                    .provides
                    .iter()
                    .any(|role| role == &request.contract)
        });
        if !declared {
            return Terminal::failed(Fault::new(
                "MissingDependency",
                "router",
                "owner does not provide action contract",
            ));
        }
        let Some(instance) = self.packages.get(package) else {
            return Terminal::failed(Fault::new(
                "MissingDependency",
                "router",
                "owner is not installed",
            ));
        };
        self.router
            .events
            .push(request.run_id, "service_called", request.public_trace());
        instance.call(request, cancel).await
    }
    /// Whether this generation has a selected role, including caller-runtime contributions.
    pub fn has_role(&self, contract: &str) -> bool {
        self.composition.roles.contains_key(contract)
    }
    /// The instance selected for one role contract, for a caller that has to
    /// know whether a role is bound to a particular package.
    pub fn role(&self, contract: &str) -> Result<Arc<NativeInstance>, Fault> {
        self.router
            .context
            .try_service::<Services>()
            .map_err(|e| Fault::new("Unavailable", "services", e.to_string()))?
            .0
            .get(contract)
            .and_then(|instance| match instance.as_ref() {
                Instance::Native(native) => Some(native.clone()),
                Instance::Local(_) => None,
            })
            .ok_or_else(|| Fault::new("MissingDependency", "native-services", contract))
    }
    /// Close admission, cancel every in-flight request, and run each instance's
    /// finalizer before releasing its library. Await this before dropping the
    /// kernel, because only it guarantees the disposal barrier was reached.
    pub async fn shutdown(&self) -> Result<(), Fault> {
        self.router.open.store(false, Ordering::Release);
        for instance in &self.instances {
            instance.close();
        }
        for cancel in self
            .router
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            cancel.cancel();
        }
        let disposed = self.fiber.dispose().await;
        // Include explicitly loaded packages that were not selected for a role;
        // instance finalizer failures survive repeated stop calls by the adapter.
        let mut cleanup: Option<Fault> = None;
        for instance in &self.instances {
            if let Err(error) = instance.stop().await {
                if let Some(first) = &mut cleanup {
                    first
                        .message
                        .push_str(&format!("; instance cleanup: {error}"));
                } else {
                    cleanup = Some(error);
                }
            }
        }
        let workers = std::mem::take(
            &mut *self
                .router
                .workers
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        for worker in workers {
            let _ = worker.await;
        }
        let mut registry = routers().lock().unwrap_or_else(|e| e.into_inner());
        for token in &self.tokens {
            registry.remove(token);
        }
        drop(registry);
        if let Some(mut error) = cleanup {
            if let Err(disposal) = disposed {
                error
                    .message
                    .push_str(&format!("; cordis cleanup: {disposal}"));
            }
            return Err(error);
        }
        disposed.map_err(|e| Fault::new("CleanupFailure", "cordis", e.to_string()))
    }
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
