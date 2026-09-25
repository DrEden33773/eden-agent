//! The active composition generation, replaced only by an explicit switch.
use super::*;
use std::sync::RwLock;
/// The current generation can be absent after a failed explicit switch.
pub(crate) struct Generation(RwLock<Option<Arc<Kernel>>>);
impl Generation {
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self(RwLock::new(None))
    }
    /// Hold a freshly loaded kernel as the active generation.
    pub fn new(kernel: Kernel) -> Self {
        Self(RwLock::new(Some(Arc::new(kernel))))
    }
    /// The active kernel, or a fault naming the explicit recovery when a failed
    /// switch left no composition installed.
    pub fn get(&self) -> Result<Arc<Kernel>, Fault> {
        self.0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| {
                Fault::new(
                    "Unavailable",
                    "composition",
                    "no active composition; explicitly restore or switch to a valid composition",
                )
            })
    }
    /// Whether a generation is installed, without failing when none is.
    pub fn available(&self) -> bool {
        self.0.read().unwrap_or_else(|e| e.into_inner()).is_some()
    }
    /// Remove the active generation, leaving the session without one until
    /// [`install`](Generation::install) supplies its replacement.
    pub fn take(&self) -> Option<Arc<Kernel>> {
        self.0.write().unwrap_or_else(|e| e.into_inner()).take()
    }
    /// Make a newly loaded kernel the active generation, replacing any current
    /// one. The replaced kernel is dropped, not shut down, so a caller switching
    /// compositions has to await its [`shutdown`](eden_kernel::Kernel::shutdown)
    /// first — dropping it does not reach the disposal barrier.
    pub fn install(&self, kernel: Kernel) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(kernel));
    }
    /// The composition the active generation mounted.
    pub fn composition(&self) -> eden_protocol::Composition {
        self.get()
            .expect("initialized generation")
            .composition()
            .clone()
    }
    /// The instance the active generation selected for one role contract.
    pub fn role(&self, contract: &str) -> Result<Arc<eden_kernel::ServiceHandle>, Fault> {
        self.get()?.role(contract)
    }
    /// Route a request through the active generation. A session with no active
    /// generation settles the request as a failure rather than panicking.
    pub async fn invoke(&self, request: Request, cancel: Cancellation) -> Terminal {
        match self.get() {
            Ok(kernel) => kernel.invoke(request, cancel).await,
            Err(error) => Terminal::failed(error),
        }
    }
    /// Shut the active generation down and leave none installed. Closing an
    /// already empty generation is not an error, so this stays idempotent.
    pub async fn shutdown(&self) -> Result<(), Fault> {
        match self.take() {
            Some(kernel) => kernel.shutdown().await,
            None => Ok(()),
        }
    }
}
