//! Controlled first-party loop, context, provider and tool using ordinary SDK roles.
use eden_plugin_sdk::{
    AgentLoop, CallContext, ContextStrategy, ModelProvider, Package, Tool,
    protocol::{self as p, Descriptor, Fault, ModelInput, ModelReply, RunInput},
    serde_json::{Value, json},
};
struct Standard;
impl AgentLoop for Standard {
    async fn run(&self, input: RunInput, cx: CallContext) -> Result<String, Fault> {
        let mut model_input = cx.context(&input).await?;
        match cx.model(&model_input).await? {
            ModelReply::Answer(answer) => Ok(answer),
            ModelReply::ToolCall(argument) => {
                model_input.tool_result = Some(cx.tool(&argument).await?);
                match cx.model(&model_input).await? {
                    ModelReply::Answer(answer) => Ok(answer),
                    ModelReply::ToolCall(_) => Err(Fault::new(
                        "ProviderFailure",
                        "controlled",
                        "expected final answer",
                    )),
                }
            }
        }
    }
}
impl ContextStrategy for Standard {
    async fn project(&self, input: RunInput, _cx: CallContext) -> Result<ModelInput, Fault> {
        Ok(ModelInput {
            text: format!("standard:{}", input.prompt),
            tool_result: None,
        })
    }
}
impl ModelProvider for Standard {
    async fn generate(&self, input: ModelInput, cx: CallContext) -> Result<ModelReply, Fault> {
        cx.emit("model_input", json!(input))?;
        match input.tool_result {
            None => Ok(ModelReply::ToolCall(input.text)),
            Some(result) => Ok(ModelReply::Answer(format!("{} => {}", input.text, result))),
        }
    }
}
impl Tool for Standard {
    async fn execute(&self, input: String, _cx: CallContext) -> Result<String, Fault> {
        Ok(format!("echo[{input}]"))
    }
}
fn descriptor() -> Descriptor {
    Descriptor {
        package: "standard".into(),
        version: "0.1.0".into(),
        provides: [p::AGENT_LOOP, p::CONTEXT, p::PROVIDER, p::TOOL]
            .iter()
            .map(|s| (*s).into())
            .collect(),
    }
}
fn create(config: Value) -> Result<Package, Fault> {
    if config.get("fail_init").and_then(Value::as_bool) == Some(true) {
        return Err(Fault::new(
            "Unavailable",
            "standard",
            "requested initialization failure",
        ));
    }
    Ok(Package::new("standard")
        .agent_loop(Standard)
        .context(Standard)
        .provider(Standard)
        .tool(Standard))
}
eden_plugin_sdk::export_plugin!(descriptor, create);
