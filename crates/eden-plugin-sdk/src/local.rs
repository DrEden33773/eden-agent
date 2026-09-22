//! Managed in-process packages use the same scopes and byte-only host callbacks as native authors.
use crate::{Cancellation, Package, Scope, abi::HostApi, author::CallContext};
use eden_protocol::{Fault, Outcome, Request, Terminal};
use futures_util::FutureExt;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

struct State {
    open: bool,
    next: u64,
    active: BTreeMap<u64, Cancellation>,
    stopped: Option<Result<(), Fault>>,
}
/// A caller-runtime package with explicit cancellation and an awaitable disposal barrier.
pub struct LocalInstance {
    package: Arc<Package>,
    host: HostApi,
    state: Mutex<State>,
    changed: tokio::sync::Notify,
    stop: tokio::sync::Mutex<()>,
}
impl LocalInstance {
    /// Mount an owned package. Host callbacks must remain valid until `stop` finishes.
    ///
    /// # Safety
    /// Every callback in `host` must implement the SDK byte ownership contract and remain callable
    /// from this runtime's workers until all accepted operations and finalization have settled.
    pub unsafe fn new(package: Package, host: HostApi) -> Arc<Self> {
        Arc::new(Self {
            package: Arc::new(package),
            host,
            state: Mutex::new(State {
                open: true,
                next: 1,
                active: BTreeMap::new(),
                stopped: None,
            }),
            changed: tokio::sync::Notify::new(),
            stop: tokio::sync::Mutex::new(()),
        })
    }
    /// Reject further work and signal every admitted operation without skipping cleanup.
    pub fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.open = false;
        for cancel in state.active.values() {
            cancel.cancel();
        }
    }
    /// Execute on the caller runtime; dropping the waiter leaves the managed worker owned.
    pub async fn call(self: &Arc<Self>, request: Request, cancel: Cancellation) -> Terminal {
        let id = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !state.open || request.contract == eden_protocol::INSTANCE_STOP {
                return Terminal::failed(Fault::new(
                    "Unavailable",
                    "local-package",
                    "admission closed",
                ));
            }
            let id = state.next;
            state.next += 1;
            state.active.insert(id, cancel.clone());
            id
        };
        let instance = self.clone();
        tokio::spawn(async move {
            let result = instance.execute(request, cancel).await;
            instance
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .active
                .remove(&id);
            instance.changed.notify_waiters();
            result
        })
        .await
        .unwrap_or_else(|e| {
            Terminal::failed(Fault::new("PluginFailure", "local-package", e.to_string()))
        })
    }
    async fn execute(&self, request: Request, cancel: Cancellation) -> Terminal {
        let scope = Scope::default();
        let context = CallContext {
            scope: scope.clone(),
            host: self.host,
            request: request.clone(),
        };
        let mut root = Box::pin(
            std::panic::AssertUnwindSafe(self.package.invoke(
                request.payload,
                &request.contract,
                context,
            ))
            .catch_unwind(),
        );
        let outcome = tokio::select! {
            biased;
            result = &mut root => match result {
                Ok(Ok(value)) => Outcome::Completed(value),
                Ok(Err(error)) => Outcome::Failed(error),
                Err(_) => Outcome::Failed(Fault::new(
                    "PluginFailure",
                    "local-package",
                    "operation panicked"
                )),
            },
            _ = cancel.cancelled() => Outcome::Cancelled,
        };
        let drop_failed =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(root))).is_err();
        let mut cleanup_errors = scope.finish().await;
        if drop_failed {
            cleanup_errors.push(Fault::new(
                "CleanupFailure",
                "local-package",
                "root destructor panicked",
            ));
        }
        Terminal {
            outcome,
            cleanup_errors,
        }
    }
    /// Cancel and drain all operations, then run the optional instance finalizer once.
    pub async fn stop(&self) -> Result<(), Fault> {
        let _stop = self.stop.lock().await;
        self.close();
        if let Some(result) = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stopped
            .clone()
        {
            return result;
        }
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .active
                .is_empty()
            {
                break;
            }
            changed.await;
        }
        let result = if self
            .package
            .descriptor
            .provides
            .iter()
            .any(|role| role == eden_protocol::INSTANCE_STOP)
        {
            self.execute(
                Request {
                    session_id: 0,
                    run_id: 0,
                    contract: eden_protocol::INSTANCE_STOP.into(),
                    payload: serde_json::Value::Null,
                },
                Cancellation::default(),
            )
            .await
            .into_result()
            .map(|_| ())
        } else {
            Ok(())
        };
        self.state.lock().unwrap_or_else(|e| e.into_inner()).stopped = Some(result.clone());
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{Bytes, Reply};
    unsafe extern "C" fn reject(_: usize, _: Bytes, _: Reply) -> u64 {
        0
    }
    unsafe extern "C" fn cancel(_: usize, _: u64) {}
    unsafe extern "C" fn event(_: usize, _: Bytes) {}
    #[tokio::test]
    async fn dropped_waiter_does_not_skip_cleanup_and_closed_proxy_rejects_work() {
        let (entered, observed) = tokio::sync::oneshot::channel();
        let entered = Arc::new(Mutex::new(Some(entered)));
        let (release, barrier) = tokio::sync::oneshot::channel();
        let barrier = Arc::new(Mutex::new(Some(barrier)));
        let package = Package::new("embedded").service("test", move |_: (), cx| {
            let entered = entered.clone();
            let barrier = barrier.clone();
            async move {
                let barrier = barrier.lock().unwrap().take().unwrap();
                cx.scope.cleanup(async move {
                    barrier.await.unwrap();
                    Ok(())
                })?;
                entered.lock().unwrap().take().unwrap().send(()).unwrap();
                std::future::pending::<Result<(), Fault>>().await
            }
        });
        // SAFETY: These static callbacks reject routing and retain no borrowed data.
        let instance = unsafe {
            LocalInstance::new(
                package,
                HostApi {
                    context: 0,
                    request: reject,
                    cancel,
                    event,
                },
            )
        };
        let request = Request {
            session_id: 1,
            run_id: 1,
            contract: "test".into(),
            payload: serde_json::Value::Null,
        };
        let client = instance.clone();
        let input = request.clone();
        let waiter = tokio::spawn(async move { client.call(input, Cancellation::default()).await });
        observed.await.unwrap();
        waiter.abort();
        let closing = instance.clone();
        let stop = tokio::spawn(async move { closing.stop().await });
        instance.close();
        assert!(
            instance
                .call(request, Cancellation::default())
                .await
                .into_result()
                .is_err()
        );
        assert!(!stop.is_finished());
        release.send(()).unwrap();
        stop.await.unwrap().unwrap();
        instance.stop().await.unwrap();
    }
}
