use eden_plugin_sdk::{AgentLoop, CallContext, Package, protocol::{self as p, Descriptor, Fault, RunInput}, serde_json::{Value, json}, tokio::{self, io::{AsyncReadExt, AsyncWriteExt}, net::TcpStream}};
use std::{future::{Future, poll_fn}, task::Poll};
struct Lifecycle { address: String, mode: String }
fn io(error: std::io::Error) -> Fault { Fault::new("ProviderFailure", "lifecycle-io", error.to_string()) }
struct PanicOnDrop(bool);
impl Drop for PanicOnDrop { fn drop(&mut self) { if self.0 { panic!("deliberate root destructor panic"); } } }
impl AgentLoop for Lifecycle {
    async fn run(&self, _: RunInput, cx: CallContext) -> Result<String, Fault> {
        let mut root = TcpStream::connect(&self.address).await.map_err(io)?;
        root.write_all(b"root\n").await.map_err(io)?;
        let address = self.address.clone();
        let cleanup_error = self.mode == "cleanup_error";
        let child_finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cleanup_observation = child_finished.clone();
        cx.scope.cleanup(async move {
            if !cleanup_observation.load(std::sync::atomic::Ordering::Acquire) {
                return Err(Fault::new("CleanupFailure", "child-barrier", "cleanup started before child completion"));
            }
            let mut stream = TcpStream::connect(address).await.map_err(io)?;
            stream.write_all(b"cleanup\n").await.map_err(io)?;
            let mut release = [0]; stream.read_exact(&mut release).await.map_err(io)?;
            stream.write_all(b"cleanup-done\n").await.map_err(io)?;
            if cleanup_error { Err(Fault::new("CleanupFailure", "author-cleanup", "deliberate cleanup failure")) } else { Ok(()) }
        })?;
        let address = self.address.clone();
        let cancellation = cx.scope.cancellation();
        let (started, started_rx) = tokio::sync::oneshot::channel();
        cx.scope.spawn(async move {
            let mut child = TcpStream::connect(address).await.map_err(io)?;
            child.write_all(b"child\n").await.map_err(io)?;
            let _ = started.send(());
            cancellation.cancelled().await;
            child.write_all(b"child-stopping\n").await.map_err(io)?;
            child.read_exact(&mut [0]).await.map_err(io)?;
            child.write_all(b"child-stopped\n").await.map_err(io)?;
            drop(child);
            child_finished.store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        })?;
        started_rx.await.map_err(|_| Fault::new("Unavailable", "child", "child did not start"))?;
        let _guard = PanicOnDrop(self.mode == "drop_panic");
        let mut byte = [0]; let read = root.read(&mut byte); tokio::pin!(read);
        let mut announced = false;
        poll_fn(|task| {
            let result = read.as_mut().poll(task);
            if result.is_pending() && !announced {
                announced = true;
                if let Err(error) = cx.emit("waiting", json!({"tcp_read": "pending"})) { return Poll::Ready(Err(std::io::Error::other(error))); }
            }
            result
        }).await.map_err(io)?;
        match self.mode.as_str() {
            "failed_then_cancel" => Err(Fault::new("ProviderFailure", "author-root", "deliberate root failure")),
            "panic" => panic!("deliberate root panic"),
            _ => Ok("io-completed".into()),
        }
    }
}
fn descriptor() -> Descriptor { Descriptor { package: "lifecycle".into(), version: "0.1.0".into(), provides: vec![p::AGENT_LOOP.into()] } }
fn create(config: Value) -> Result<Package, Fault> {
    let address = config.get("address").and_then(Value::as_str).ok_or_else(|| Fault::new("InvalidInput", "lifecycle", "address required"))?.to_owned();
    let mode = config.get("mode").and_then(Value::as_str).unwrap_or("cancel").to_owned();
    Ok(Package::new("lifecycle").agent_loop(Lifecycle { address, mode }))
}
eden_plugin_sdk::export_plugin!(descriptor, create);
