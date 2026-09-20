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
    entries: Mutex<Vec<Event>>,
    changed: tokio::sync::Notify,
}
impl Events {
    pub fn new(session_id: u64) -> Arc<Self> {
        Arc::new(Self {
            session_id,
            entries: Mutex::new(vec![]),
            changed: tokio::sync::Notify::new(),
        })
    }
    pub fn push(&self, run_id: u64, kind: &str, payload: serde_json::Value) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let sequence = entries.len() as u64 + 1;
        entries.push(Event {
            sequence,
            session_id: self.session_id,
            run_id,
            kind: kind.into(),
            payload,
        });
        self.changed.notify_waiters();
    }
    pub fn snapshot(&self) -> Vec<Event> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub async fn after(&self, sequence: u64) -> Vec<Event> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let events: Vec<_> = self
                .snapshot()
                .into_iter()
                .filter(|e| e.sequence > sequence)
                .collect();
            if !events.is_empty() {
                return events;
            }
            changed.await;
        }
    }
}
struct Services(BTreeMap<String, Arc<NativeInstance>>);
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
        self.events.push(
            request.run_id,
            "service_called",
            serde_json::json!({ "contract": request.contract, "input": request.payload }),
        );
        instance.call(request, cancel).await
    }
}
// HostApi tokens use a process registry, so retained author contexts cannot dereference freed hosts.
static ROUTERS: std::sync::OnceLock<Mutex<BTreeMap<usize, std::sync::Weak<Router>>>> =
    std::sync::OnceLock::new();
static NEXT_ROUTER: AtomicU64 = AtomicU64::new(1);
fn routers() -> &'static Mutex<BTreeMap<usize, std::sync::Weak<Router>>> {
    ROUTERS.get_or_init(Mutex::default)
}
fn router(id: usize) -> Option<Arc<Router>> {
    routers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .and_then(std::sync::Weak::upgrade)
}
unsafe extern "C" fn request(context: usize, bytes: Bytes, reply: Reply) -> u64 {
    let Some(router) = router(context) else {
        return 0;
    };
    if !router.open.load(Ordering::Acquire) {
        return 0;
    }
    // SAFETY: SDK request bytes are kept live during the call and copied by decode.
    let input = unsafe { bytes.decode::<Request>() };
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
    token: usize,
    fiber: FiberHandle,
    instances: Vec<Arc<NativeInstance>>,
    composition: Composition,
}
async fn rollback_initialization(mut error: Fault, instances: &[Arc<NativeInstance>]) -> Fault {
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
        let base = base.to_owned();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = Self::load_owned(composition, &base, session_id, events).await;
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
    ) -> Result<Self, Fault> {
        preflight(&composition)?;
        // Resolve all paths before executing the first native library.
        let paths: Vec<_> = composition
            .packages
            .iter()
            .map(|p| {
                std::fs::canonicalize(base.join(&p.library)).map_err(|e| {
                    Fault::new("MissingDependency", &p.descriptor.package, e.to_string())
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
        let token = NEXT_ROUTER.fetch_add(1, Ordering::Relaxed) as usize;
        routers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(token, Arc::downgrade(&router));
        let host = HostApi {
            context: token,
            request,
            cancel,
            event,
        };
        let mut packages: BTreeMap<String, Arc<NativeInstance>> = BTreeMap::new();
        for (manifest, path) in composition.packages.iter().zip(paths) {
            let manifest = manifest.clone();
            let name = manifest.descriptor.package.clone();
            let result =
                tokio::task::spawn_blocking(move || NativeInstance::load(&path, &manifest, host))
                    .await
                    .map_err(|e| Fault::new("Unavailable", "initialize", e.to_string()))
                    .and_then(|r| r);
            match result {
                Ok(instance) => {
                    packages.insert(name, instance);
                }
                Err(error) => {
                    router.open.store(false, Ordering::Release);
                    let instances: Vec<_> = packages.values().cloned().collect();
                    let error = rollback_initialization(error, &instances).await;
                    routers()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&token);
                    return Err(error);
                }
            }
        }
        let instances: Vec<_> = packages.values().cloned().collect();
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
                routers()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&token);
                return Err(error);
            }
        };
        Ok(Self {
            router,
            token,
            fiber,
            instances,
            composition,
        })
    }
    pub fn composition(&self) -> &Composition {
        &self.composition
    }
    pub async fn invoke(&self, request: Request, cancel: Cancellation) -> Terminal {
        self.router.invoke(request, cancel).await
    }
    pub fn role(&self, contract: &str) -> Result<Arc<NativeInstance>, Fault> {
        self.router
            .context
            .try_service::<Services>()
            .map_err(|e| Fault::new("Unavailable", "services", e.to_string()))?
            .0
            .get(contract)
            .cloned()
            .ok_or_else(|| Fault::new("MissingDependency", "services", contract))
    }
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
        routers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.token);
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
