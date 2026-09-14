use eden_plugin_sdk::{
    AgentLoop, CallContext, Package,
    protocol::{self as p, Descriptor, Fault, ModelReply, RunInput},
    serde_json::Value,
};
struct LoopA;
impl AgentLoop for LoopA {
    async fn run(&self, input: RunInput, cx: CallContext) -> Result<String, Fault> {
        // Unlike the default loop, this author runs its tool before calling the provider.
        let mut model_input = cx.context(&input).await?;
        model_input.tool_result = Some(cx.tool(&format!("A:{}", model_input.text)).await?);
        match cx.model(&model_input).await? {
            ModelReply::Answer(answer) => Ok(format!("A/{answer}")),
            ModelReply::ToolCall(_) => Err(Fault::new(
                "ProviderFailure",
                "loop-a",
                "unexpected tool request",
            )),
        }
    }
}
fn descriptor() -> Descriptor {
    Descriptor {
        package: "loop-a".into(),
        version: "0.1.0".into(),
        provides: vec![p::AGENT_LOOP.into()],
    }
}
fn create(_: Value) -> Result<Package, Fault> {
    Ok(Package::new("loop-a").agent_loop(LoopA))
}
eden_plugin_sdk::export_plugin!(descriptor, create);
