use eden_plugin_sdk::{
    AgentLoop, CallContext, Package,
    protocol::{self as p, Descriptor, Fault, RunInput},
    serde_json::Value,
};
use std::{
    io::{Read, Write},
    net::TcpStream,
};
struct InitProbe {
    stream: Option<TcpStream>,
    marker: Option<String>,
}
impl Drop for InitProbe {
    fn drop(&mut self) {
        if let Some(stream) = &mut self.stream {
            let _ = stream.write_all(b"destroyed\n");
        }
        if let Some(marker) = &self.marker {
            let _ = std::fs::write(marker, "destroyed");
        }
    }
}
impl AgentLoop for InitProbe {
    async fn run(&self, _: RunInput, _: CallContext) -> Result<String, Fault> {
        Ok("probe-completed".into())
    }
}
fn descriptor() -> Descriptor {
    Descriptor {
        package: "init-probe".into(),
        version: "0.1.0".into(),
        provides: vec![p::AGENT_LOOP.into()],
    }
}
fn create(config: Value) -> Result<Package, Fault> {
    let stream = if let Some(address) = config.get("address").and_then(Value::as_str) {
        let connect = || -> Result<TcpStream, std::io::Error> {
            let mut stream = TcpStream::connect(address)?;
            stream.write_all(b"initializing\n")?;
            stream.read_exact(&mut [0])?;
            Ok(stream)
        };
        Some(connect().map_err(|e| Fault::new("Unavailable", "init-probe", e.to_string()))?)
    } else {
        None
    };
    let marker = config
        .get("marker")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(Package::new("init-probe").agent_loop(InitProbe { stream, marker }))
}
eden_plugin_sdk::export_plugin!(descriptor, create);
