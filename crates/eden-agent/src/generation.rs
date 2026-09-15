use super::*;
use std::sync::RwLock;
/// The current generation can be absent after a failed explicit switch.
pub(crate) struct Generation(RwLock<Option<Arc<Kernel>>>);
impl Generation {
    pub fn new(kernel: Kernel) -> Self {
        Self(RwLock::new(Some(Arc::new(kernel))))
    }
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
    pub fn available(&self) -> bool {
        self.0.read().unwrap_or_else(|e| e.into_inner()).is_some()
    }
    pub fn take(&self) -> Option<Arc<Kernel>> {
        self.0.write().unwrap_or_else(|e| e.into_inner()).take()
    }
    pub fn install(&self, kernel: Kernel) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(kernel));
    }
    pub fn composition(&self) -> eden_protocol::Composition {
        self.get()
            .expect("initialized generation")
            .composition()
            .clone()
    }
    pub fn role(&self, contract: &str) -> Result<Arc<eden_kernel::NativeInstance>, Fault> {
        self.get()?.role(contract)
    }
    pub async fn invoke(&self, request: Request, cancel: Cancellation) -> Terminal {
        match self.get() {
            Ok(kernel) => kernel.invoke(request, cancel).await,
            Err(error) => Terminal::failed(error),
        }
    }
    pub async fn shutdown(&self) -> Result<(), Fault> {
        match self.take() {
            Some(kernel) => kernel.shutdown().await,
            None => Ok(()),
        }
    }
}
