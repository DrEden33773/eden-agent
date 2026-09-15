//! Operation-owned asynchronous work and cleanup.
use eden_protocol::Fault;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;
/// A future owned by this library, never passed across the ABI.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
/// Cooperative cancellation signal.
#[derive(Clone)]
pub struct Cancellation(watch::Sender<bool>);
impl Default for Cancellation {
    fn default() -> Self {
        Self(watch::channel(false).0)
    }
}
impl Cancellation {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    pub async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}
struct State {
    open: bool,
    children: Vec<tokio::task::JoinHandle<Result<(), Fault>>>,
    cleanups: Vec<BoxFuture<Result<(), Fault>>>,
}
/// One operation's child admission and cleanup owner.
#[derive(Clone)]
pub struct Scope {
    state: Arc<Mutex<State>>,
    cancellation: Cancellation,
}
impl Default for Scope {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                open: true,
                children: vec![],
                cleanups: vec![],
            })),
            cancellation: Cancellation::default(),
        }
    }
}
impl Scope {
    pub(crate) fn while_open<T>(&self, callback: impl FnOnce() -> T) -> Result<T, Fault> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.open {
            return Err(Fault::new("Unavailable", "scope", "operation closed"));
        }
        Ok(callback())
    }
    pub fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }
    pub fn spawn(
        &self,
        future: impl Future<Output = Result<(), Fault>> + Send + 'static,
    ) -> Result<(), Fault> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.open {
            return Err(Fault::new("Unavailable", "scope", "child admission closed"));
        }
        state.children.push(tokio::spawn(future));
        Ok(())
    }
    pub fn cleanup(
        &self,
        future: impl Future<Output = Result<(), Fault>> + Send + 'static,
    ) -> Result<(), Fault> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.open {
            return Err(Fault::new(
                "Unavailable",
                "scope",
                "cleanup admission closed",
            ));
        }
        state.cleanups.push(Box::pin(future));
        Ok(())
    }
    pub(crate) async fn finish(&self) -> Vec<Fault> {
        use futures_util::FutureExt;
        let (children, cleanups) = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.open = false;
            (
                std::mem::take(&mut state.children),
                std::mem::take(&mut state.cleanups),
            )
        };
        self.cancellation.cancel();
        let mut errors = vec![];
        for child in children {
            match child.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(Fault::new(
                    "CleanupFailure",
                    error.source.as_str(),
                    error.to_string(),
                )),
                Err(error) => errors.push(Fault::new("CleanupFailure", "child", error.to_string())),
            }
        }
        for cleanup in cleanups {
            let mut guarded = Box::pin(std::panic::AssertUnwindSafe(cleanup).catch_unwind());
            let result = guarded.as_mut().await;
            let drop_failed =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(guarded))).is_err();
            if drop_failed {
                errors.push(Fault::new(
                    "CleanupFailure",
                    "cleanup",
                    "cleanup destructor panicked",
                ));
            }
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(Fault::new(
                    "CleanupFailure",
                    error.source.as_str(),
                    error.to_string(),
                )),
                Err(_) => errors.push(Fault::new("CleanupFailure", "cleanup", "cleanup panicked")),
            }
        }
        errors
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn finish_waits_for_cleanup_and_closes_child_admission() {
        let scope = Scope::default();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        scope
            .cleanup(async move {
                entered_tx.send(()).unwrap();
                release_rx.await.unwrap();
                Ok(())
            })
            .unwrap();
        let finalizing = scope.clone();
        let barrier = tokio::spawn(async move { finalizing.finish().await });
        tokio::select! {
            result = entered_rx => result.unwrap(),
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                panic!("cleanup was never entered");
            }
        }
        assert!(
            !barrier.is_finished(),
            "finish returned before cleanup release"
        );
        assert_eq!(
            scope.spawn(async { Ok(()) }).unwrap_err().code,
            "Unavailable"
        );
        release_tx.send(()).unwrap();
        assert!(barrier.await.unwrap().is_empty());
    }
}

#[cfg(test)]
mod destructor_tests {
    use super::*;
    struct CleanupDropPanic;
    impl Future for CleanupDropPanic {
        type Output = Result<(), Fault>;
        fn poll(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            std::task::Poll::Ready(Ok(()))
        }
    }
    impl Drop for CleanupDropPanic {
        fn drop(&mut self) {
            panic!("cleanup destructor");
        }
    }
    #[tokio::test]
    async fn cleanup_destructor_panic_does_not_skip_later_cleanup() {
        let scope = Scope::default();
        let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
        scope.cleanup(CleanupDropPanic).unwrap();
        let observed = reached.clone();
        scope
            .cleanup(async move {
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
            .unwrap();
        let errors = scope.finish().await;
        assert!(reached.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, "CleanupFailure");
    }
}
