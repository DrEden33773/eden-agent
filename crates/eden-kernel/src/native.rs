//! Owns native handles; all calls pass the admission gate.
use eden_plugin_sdk::{
    Cancellation,
    abi::{Api, Bytes, Header, HostApi, Reply, validate_header},
};
use eden_protocol::{Descriptor, Fault, PackageManifest, Request, Terminal};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, oneshot};

struct Gate {
    handle: Option<usize>,
    open: bool,
    active: usize,
    operations: std::collections::BTreeSet<u64>,
}
/// A generation-bound native role proxy. Old proxies reject work after shutdown.
pub struct NativeInstance {
    api: &'static Api,
    gate: Mutex<Gate>,
    drained: Notify,
    stop_lock: tokio::sync::Mutex<()>,
    has_finalizer: bool,
    stop_result: Mutex<Option<Result<(), Fault>>>,
}
fn failure(message: impl Into<String>) -> Fault {
    Fault::new("Unavailable", "loader", message)
}
unsafe extern "C" fn capture(context: usize, bytes: Bytes) {
    // SAFETY: The synchronous caller owns this Vec for the duration of the callback.
    let output = unsafe { &mut *(context as *mut Vec<u8>) };
    // SAFETY: The exporting library keeps bytes live until this callback returns.
    let decoded = if bytes.len == 0 {
        &[]
    } else {
        // SAFETY: The exporting library borrows this span until capture returns.
        unsafe { std::slice::from_raw_parts(bytes.ptr, bytes.len) }
    };
    output.extend_from_slice(decoded);
}
fn sink(output: &mut Vec<u8>) -> Reply {
    Reply {
        context: output as *mut Vec<u8> as usize,
        call: capture,
    }
}
unsafe extern "C" fn complete(context: usize, bytes: Bytes) {
    // SAFETY: start received this uniquely allocated sender and completes once.
    let sender = unsafe { Box::from_raw(context as *mut oneshot::Sender<Terminal>) };
    // SAFETY: Completion data is borrowed only for this callback.
    let result = unsafe { bytes.decode() }.unwrap_or_else(Terminal::failed);
    let _ = sender.send(result);
}
impl NativeInstance {
    pub(crate) fn load(
        path: &std::path::Path,
        manifest: &PackageManifest,
        host: HostApi,
    ) -> Result<Arc<Self>, Fault> {
        // SAFETY: Only explicitly enabled trusted local code is loaded; manifests were preflighted.
        let library =
            unsafe { libloading::Library::new(path) }.map_err(|e| failure(e.to_string()))?;
        // Code stays resident even on failed handshake: loading may have initialized native state.
        let library = Box::leak(Box::new(library));
        // SAFETY: The entry symbol is the versioned prefix-only ABI contract.
        let entry =
            unsafe { library.get::<unsafe extern "C" fn() -> *const Header>(b"eden_plugin_v1\0") }
                .map_err(|e| failure(e.to_string()))?;
        // SAFETY: A conforming trusted entry returns a readable Header or null.
        let pointer = unsafe { entry() };
        if pointer.is_null() {
            return Err(failure("null ABI header"));
        }
        // SAFETY: Only the fixed prefix is read before checking table size and version.
        validate_header(unsafe { &*pointer }, std::mem::size_of::<Api>())?;
        // SAFETY: Header matching proves the exact agreed table layout; library is process-resident.
        let api = unsafe { &*(pointer as *const Api) };
        let mut metadata = vec![];
        // SAFETY: describe is synchronous and borrows the live local capture receiver.
        unsafe {
            (api.describe)(sink(&mut metadata));
        }
        let descriptor: Result<Descriptor, Fault> =
            serde_json::from_slice(&metadata).map_err(|e| failure(e.to_string()))?;
        if descriptor? != manifest.descriptor {
            return Err(Fault::new(
                "IncompatibleContract",
                "loader",
                "library descriptor differs from manifest",
            ));
        }
        let config = serde_json::to_vec(&manifest.config).map_err(|e| failure(e.to_string()))?;
        let mut diagnostic = vec![];
        // SAFETY: Host callbacks outlive this instance, and create consumes config synchronously.
        let handle = unsafe { (api.create)(host, Bytes::new(&config), sink(&mut diagnostic)) };
        if handle == 0 {
            return Err(serde_json::from_slice(&diagnostic)
                .unwrap_or_else(|_| failure("initialization failed without a diagnostic")));
        }
        Ok(Arc::new(Self {
            api,
            gate: Mutex::new(Gate {
                handle: Some(handle),
                open: true,
                active: 0,
                operations: Default::default(),
            }),
            drained: Notify::new(),
            stop_lock: tokio::sync::Mutex::new(()),
            has_finalizer: manifest
                .descriptor
                .provides
                .iter()
                .any(|role| role == eden_protocol::INSTANCE_STOP),
            stop_result: Mutex::new(None),
        }))
    }
    /// Stop admitting immediately; actual resource shutdown is awaited separately.
    pub fn close(&self) {
        let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        gate.open = false;
        if let Some(handle) = gate.handle {
            for id in &gate.operations {
                // SAFETY: Gate holds the live instance; cancel only signals each retained operation.
                unsafe {
                    (self.api.cancel)(handle, *id);
                }
            }
        }
    }
    /// Calls retain their own worker even when the caller drops its receiver.
    pub async fn call(self: &Arc<Self>, request: Request, cancel: Cancellation) -> Terminal {
        let instance = self.clone();
        match tokio::spawn(async move { instance.invoke(request, cancel, false).await }).await {
            Ok(terminal) => terminal,
            Err(error) => Terminal::failed(failure(error.to_string())),
        }
    }
    async fn invoke(
        self: Arc<Self>,
        request: Request,
        cancel: Cancellation,
        finalizer: bool,
    ) -> Terminal {
        if !finalizer && request.contract == eden_protocol::INSTANCE_STOP {
            return Terminal::failed(Fault::new(
                "Unavailable",
                "instance",
                "instance finalization is reserved for host shutdown",
            ));
        }
        let bytes = match serde_json::to_vec(&request) {
            Ok(bytes) => bytes,
            Err(e) => return Terminal::failed(failure(e.to_string())),
        };
        let (sender, receiver) = oneshot::channel();
        let (handle, operation) = {
            let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            let Some(handle) = gate.handle.filter(|_| gate.open || finalizer) else {
                return Terminal::failed(Fault::new(
                    "Unavailable",
                    "instance",
                    "instance admission closed",
                ));
            };
            gate.active += 1;
            let reply = Reply {
                context: Box::into_raw(Box::new(sender)) as usize,
                call: complete,
            };
            // SAFETY: The admission gate excludes destroy; the callback owns the allocated sender.
            let operation = unsafe { (self.api.start)(handle, Bytes::new(&bytes), reply) };
            gate.operations.insert(operation);
            (handle, operation)
        };
        let mut receiver = receiver;
        let terminal = tokio::select! {
            biased;
            result = &mut receiver => result,
            _ = cancel.cancelled() => {
                // SAFETY: This call is counted active until release finishes.
                unsafe {
                    (self.api.cancel)(handle, operation);
                }
                receiver.await
            }
        }
        .unwrap_or_else(|_| Terminal::failed(failure("native completion lost")));
        let api = self.api;
        let release = tokio::task::spawn_blocking(move || {
            // SAFETY: Callback completed, handle remains counted active; no other release uses this id.
            unsafe {
                (api.release)(handle, operation);
            }
        })
        .await;
        {
            let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            gate.active -= 1;
            gate.operations.remove(&operation);
        }
        self.drained.notify_waiters();
        match release {
            Ok(()) => terminal,
            Err(e) => Terminal::failed(failure(e.to_string())),
        }
    }
    pub(crate) async fn stop(self: &Arc<Self>) -> Result<(), Fault> {
        let _owner = self.stop_lock.lock().await;
        if let Some(result) = self
            .stop_result
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return result;
        }
        self.close();
        loop {
            let notified = self.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.gate.lock().unwrap_or_else(|e| e.into_inner()).active == 0 {
                break;
            }
            notified.await;
        }
        let mut result = if self.has_finalizer {
            self.clone()
                .invoke(
                    Request {
                        session_id: 0,
                        run_id: 0,
                        contract: eden_protocol::INSTANCE_STOP.into(),
                        payload: serde_json::Value::Null,
                    },
                    Cancellation::default(),
                    true,
                )
                .await
                .into_result()
                .map(|_| ())
        } else {
            Ok(())
        };
        let handle = self
            .gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .handle
            .take();
        if let Some(handle) = handle {
            let api = self.api;
            let destroyed = tokio::task::spawn_blocking(move || {
                // SAFETY: Admission is closed, all operations released, and this is the sole destroy owner.
                unsafe {
                    (api.destroy)(handle);
                }
            })
            .await;
            if let Err(error) = destroyed {
                result = Err(Fault::new("CleanupFailure", "destroy", error.to_string()));
            }
        }
        *self.stop_result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result.clone());
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static FINALIZERS: AtomicUsize = AtomicUsize::new(0);
    static DESTROYED: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn describe(_: Reply) {}
    unsafe extern "C" fn create(_: HostApi, _: Bytes, _: Reply) -> usize {
        1
    }
    unsafe extern "C" fn start(_: usize, bytes: Bytes, reply: Reply) -> u64 {
        // SAFETY: NativeInstance keeps the serialized request live throughout start.
        let request: Request = unsafe { bytes.decode() }.unwrap();
        assert_eq!(request.contract, eden_protocol::INSTANCE_STOP);
        FINALIZERS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: This mock consumes the accepted receiver exactly once.
        unsafe {
            reply.send(&Terminal::failed(Fault::new(
                "CleanupFailure",
                "fixture",
                "observable stop failure",
            )));
        }
        1
    }
    unsafe extern "C" fn cancel(_: usize, _: u64) {}
    unsafe extern "C" fn release(_: usize, _: u64) {}
    unsafe extern "C" fn destroy(_: usize) {
        DESTROYED.fetch_add(1, Ordering::SeqCst);
    }
    static API: Api = Api {
        header: Header {
            magic: [0; 8],
            abi: 1,
            size: 0,
            sdk: [0; 32],
            target: [0; 64],
        },
        describe,
        create,
        start,
        cancel,
        release,
        destroy,
    };
    #[tokio::test]
    async fn stop_finalizes_once_reports_failure_and_keeps_admission_closed() {
        let instance = Arc::new(NativeInstance {
            api: &API,
            gate: Mutex::new(Gate {
                handle: Some(1),
                open: true,
                active: 0,
                operations: Default::default(),
            }),
            drained: Notify::new(),
            stop_lock: tokio::sync::Mutex::new(()),
            has_finalizer: true,
            stop_result: Mutex::new(None),
        });
        assert_eq!(
            instance
                .call(
                    Request {
                        session_id: 1,
                        run_id: 1,
                        contract: eden_protocol::INSTANCE_STOP.into(),
                        payload: serde_json::Value::Null
                    },
                    Cancellation::default()
                )
                .await
                .into_result()
                .unwrap_err()
                .code,
            "Unavailable"
        );
        assert_eq!(FINALIZERS.load(Ordering::SeqCst), 0);
        assert_eq!(instance.stop().await.unwrap_err().code, "CleanupFailure");
        assert_eq!(instance.stop().await.unwrap_err().code, "CleanupFailure");
        assert_eq!(FINALIZERS.load(Ordering::SeqCst), 1);
        assert_eq!(DESTROYED.load(Ordering::SeqCst), 1);
        let terminal = instance
            .call(
                Request {
                    session_id: 1,
                    run_id: 1,
                    contract: eden_protocol::INSTANCE_STOP.into(),
                    payload: serde_json::Value::Null,
                },
                Cancellation::default(),
            )
            .await;
        assert_eq!(terminal.into_result().unwrap_err().code, "Unavailable");
    }

    struct RollbackFixture {
        source: &'static str,
        finalized: Arc<AtomicUsize>,
        destroyed: Arc<AtomicUsize>,
    }
    unsafe extern "C" fn rollback_start(handle: usize, bytes: Bytes, reply: Reply) -> u64 {
        // SAFETY: The test owns this fixture until the host's final destroy barrier.
        let fixture = unsafe { &*(handle as *const RollbackFixture) };
        // SAFETY: NativeInstance borrows its serialized request for this synchronous call.
        let request: Request = unsafe { bytes.decode() }.unwrap();
        assert_eq!(request.contract, eden_protocol::INSTANCE_STOP);
        fixture.finalized.fetch_add(1, Ordering::SeqCst);
        // SAFETY: This fixture completes the accepted receiver exactly once.
        unsafe {
            reply.send(&Terminal::failed(Fault::new(
                "CleanupFailure",
                fixture.source,
                "rollback finalizer failed",
            )));
        }
        1
    }
    unsafe extern "C" fn rollback_destroy(handle: usize) {
        // SAFETY: Host destruction takes unique ownership after all operations have released.
        let fixture = unsafe { Box::from_raw(handle as *mut RollbackFixture) };
        fixture.destroyed.fetch_add(1, Ordering::SeqCst);
    }
    static ROLLBACK_API: Api = Api {
        start: rollback_start,
        destroy: rollback_destroy,
        ..API
    };
    #[tokio::test]
    async fn initialization_rollback_preserves_all_failures_and_destroys_every_instance() {
        let finalized = Arc::new(AtomicUsize::new(0));
        let destroyed = Arc::new(AtomicUsize::new(0));
        let instances: Vec<_> = ["first-plugin", "second-plugin"]
            .into_iter()
            .map(|source| {
                let fixture = Box::new(RollbackFixture {
                    source,
                    finalized: finalized.clone(),
                    destroyed: destroyed.clone(),
                });
                Arc::new(NativeInstance {
                    api: &ROLLBACK_API,
                    gate: Mutex::new(Gate {
                        handle: Some(Box::into_raw(fixture) as usize),
                        open: true,
                        active: 0,
                        operations: Default::default(),
                    }),
                    drained: Notify::new(),
                    stop_lock: tokio::sync::Mutex::new(()),
                    has_finalizer: true,
                    stop_result: Mutex::new(None),
                })
            })
            .collect();
        let original = Fault::new("Unavailable", "third-plugin", "initialization rejected");
        let error = crate::rollback_initialization(
            original.clone(),
            &instances
                .iter()
                .map(|i| Arc::new(crate::Instance::Native(i.clone())))
                .collect::<Vec<_>>(),
        )
        .await;
        assert_eq!(error.code, original.code);
        assert_eq!(error.source, original.source);
        assert!(error.message.starts_with(&original.message));
        for source in ["first-plugin", "second-plugin"] {
            assert!(
                error
                    .message
                    .contains(&format!("CleanupFailure ({source})"))
            );
        }
        assert_eq!(finalized.load(Ordering::SeqCst), 2);
        assert_eq!(destroyed.load(Ordering::SeqCst), 2);
        for instance in instances {
            let gate = instance.gate.lock().unwrap();
            assert!(!gate.open);
            assert!(gate.handle.is_none());
        }
    }
}
