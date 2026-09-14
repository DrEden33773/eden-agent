#[cfg(any(feature = "wrong-abi", feature = "wrong-sdk", feature = "short-table"))]
#[unsafe(no_mangle)]
pub extern "C" fn eden_plugin_v1() -> *const eden_plugin_sdk::abi::Header {
    use eden_plugin_sdk::abi::{Api, Header, TARGET, field};
    static HEADER: Header = Header {
        magic: *b"EDENABI\0",
        abi: if cfg!(feature = "wrong-abi") { 99 } else { 1 },
        size: if cfg!(feature = "short-table") { std::mem::size_of::<Header>() as u32 } else { std::mem::size_of::<Api>() as u32 },
        sdk: field(if cfg!(feature = "wrong-sdk") { "wrong-sdk" } else { eden_plugin_sdk::protocol::CONTRACT }),
        target: field(TARGET),
    };
    &HEADER
}
#[cfg(not(any(feature = "wrong-abi", feature = "wrong-sdk", feature = "short-table")))]
mod valid_table {
    use eden_plugin_sdk::{AgentLoop, CallContext, Package, protocol::{self as p, Descriptor, Fault, RunInput}, serde_json::Value};
    struct Invalid;
    impl AgentLoop for Invalid {
        async fn run(&self, _: RunInput, _: CallContext) -> Result<String, Fault> { Ok("unused".into()) }
    }
    fn descriptor() -> Descriptor {
        assert!(!cfg!(feature = "metadata-panic"), "deliberate metadata panic");
        Descriptor { package: "invalid".into(), version: "0.1.0".into(), provides: vec![p::AGENT_LOOP.into()] }
    }
    fn create(_: Value) -> Result<Package, Fault> {
        assert!(!cfg!(feature = "init-panic"), "deliberate initialization panic");
        Ok(Package::new("invalid").agent_loop(Invalid))
    }
    eden_plugin_sdk::export_plugin!(descriptor, create);
}
