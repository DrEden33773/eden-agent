//! Library-owned runtime and ABI trampolines. Used by `export_plugin!`.
use crate::{
    abi::{Bytes, HostApi, Reply},
    author::{CallContext, Package},
    scope::{Cancellation, Scope},
};
use eden_protocol::{Descriptor, Fault, Outcome, Request, Terminal};
use futures_util::FutureExt;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
struct Operation {
    cancel: Cancellation,
    task: tokio::task::JoinHandle<()>,
}
struct Instance {
    runtime: tokio::runtime::Runtime,
    package: Arc<Package>,
    host: HostApi,
    operations: Mutex<BTreeMap<u64, Operation>>,
}
static NEXT: AtomicU64 = AtomicU64::new(1);

/// # Safety
/// `reply` is a live single-use receiver token; `config` is readable until return.
pub unsafe fn create(
    factory: fn(Value) -> Result<Package, Fault>,
    descriptor: fn() -> Descriptor,
    host: HostApi,
    config: Bytes,
    reply: Reply,
) -> usize {
    let result = std::panic::catch_unwind(|| {
        // SAFETY: The host owns the config span during create.
        let config = unsafe { config.decode::<Value>() }?;
        let package = factory(config)?;
        if package.descriptor != descriptor() {
            return Err(Fault::new(
                "IncompatibleContract",
                "plugin",
                "factory and exported descriptor differ",
            ));
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| Fault::new("Unavailable", "runtime", e.to_string()))?;
        Ok(Box::into_raw(Box::new(Instance {
            runtime,
            package: Arc::new(package),
            host,
            operations: Mutex::new(BTreeMap::new()),
        })) as usize)
    })
    .unwrap_or_else(|_| {
        Err(Fault::new(
            "PluginFailure",
            "initialize",
            "initializer panicked",
        ))
    });
    match result {
        Ok(instance) => instance,
        Err(error) => {
            // SAFETY: The host supplied an error receiver for this synchronous create.
            unsafe {
                reply.send(&error);
            }
            0
        }
    }
}
/// # Safety
/// The live instance owns every returned operation; destroy cannot race this call.
pub unsafe extern "C" fn start(instance: usize, bytes: Bytes, reply: Reply) -> u64 {
    // SAFETY: The host holds its native instance admission lock while calling start.
    let instance = unsafe { &*(instance as *const Instance) };
    // SAFETY: The request bytes are borrowed for the duration of start.
    let request: Request = match unsafe { bytes.decode() } {
        Ok(request) => request,
        Err(error) => {
            // SAFETY: A completion receiver is consumed even for invalid input.
            unsafe {
                reply.send(&Terminal::failed(error));
            }
            return 0;
        }
    };
    let cancel = Cancellation::default();
    let operation_cancel = cancel.clone();
    let package = instance.package.clone();
    let host = instance.host;
    let task = instance.runtime.spawn(async move {
        let scope = Scope::default();
        let context = CallContext { scope: scope.clone(), host, request: request.clone() };
        let mut root = Box::pin(std::panic::AssertUnwindSafe(package.invoke(request.payload, &request.contract, context)).catch_unwind());
        let outcome = {
            tokio::select! {
                biased;
                result = &mut root => match result {
                    Ok(Ok(value)) => Outcome::Completed(value),
                    Ok(Err(error)) => Outcome::Failed(error),
                    Err(_) => Outcome::Failed(Fault::new("PluginFailure", &package.descriptor.package, "operation panicked")),
                },
                _ = operation_cancel.cancelled() => Outcome::Cancelled,
            }
        };
        let drop_failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(root))).is_err();
        let mut cleanup_errors = scope.finish().await;
        if drop_failed { cleanup_errors.push(Fault::new("CleanupFailure", "root", "root destructor panicked")); }
        // SAFETY: Host keeps the receiver live until this one completion; spans are copied by it.
        unsafe { reply.send(&Terminal { outcome, cleanup_errors }); }
    });
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    instance
        .operations
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, Operation { cancel, task });
    id
}
/// # Safety
/// The instance remains live. Cancellation of absent or completed ids is harmless.
pub unsafe extern "C" fn cancel(instance: usize, id: u64) {
    // SAFETY: Native instance lifetime is held by the host proxy.
    let instance = unsafe { &*(instance as *const Instance) };
    if let Some(operation) = instance
        .operations
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
    {
        operation.cancel.cancel();
    }
}
/// # Safety
/// Called on a non-runtime thread after completion, once per operation.
pub unsafe extern "C" fn release(instance: usize, id: u64) {
    // SAFETY: The host waits for release before allowing instance destruction.
    let instance = unsafe { &*(instance as *const Instance) };
    let operation = instance
        .operations
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
    if let Some(operation) = operation {
        let _ = instance.runtime.block_on(operation.task);
    }
}
/// # Safety
/// Called once, on a non-runtime thread, after admission closes and all host calls finish.
pub unsafe extern "C" fn destroy(instance: usize) {
    // SAFETY: create allocated this Box; the host transfers exclusive ownership back exactly once.
    let instance = unsafe { Box::from_raw(instance as *mut Instance) };
    let operations = std::mem::take(
        &mut *instance
            .operations
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );
    for operation in operations.values() {
        operation.cancel.cancel();
    }
    for (_, operation) in operations {
        let _ = instance.runtime.block_on(operation.task);
    }
    drop(instance);
}
