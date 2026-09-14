use eden_plugin_sdk::{CallContext, ContextStrategy, Package, protocol::{self as p, Descriptor, Fault, ModelInput, RunInput}, serde_json::Value};
struct ContextB;
impl ContextStrategy for ContextB {
    async fn project(&self, input: RunInput, _: CallContext) -> Result<ModelInput, Fault> { Ok(ModelInput { text: format!("B<{}>", input.prompt.to_uppercase()), tool_result: None }) }
}
fn descriptor() -> Descriptor { Descriptor { package: "context-b".into(), version: "0.1.0".into(), provides: vec![p::CONTEXT.into()] } }
fn create(_: Value) -> Result<Package, Fault> { Ok(Package::new("context-b").context(ContextB)) }
eden_plugin_sdk::export_plugin!(descriptor, create);
