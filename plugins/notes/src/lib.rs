//! Optional notes preparation and recovery through the public checkpoint contract.
use eden_plugin_sdk::{
    CallContext, Package, protocol as p,
    serde_json::{self, Value, json},
};
use p::{Fault, coding::*, compaction as c};
use serde::Deserialize;
const NAMESPACE: &str = "eden.notes";
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    max_output_tokens: u32,
    timeout_ms: u64,
    max_input_bytes: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            max_output_tokens: 2048,
            timeout_ms: 60_000,
            max_input_bytes: 262_144,
        }
    }
}
fn fault(code: &str, message: &str) -> Fault {
    Fault::new(code, "notes", message)
}
fn config(mut value: Value) -> Result<Config, Fault> {
    if let Some(object) = value.as_object_mut() {
        object.remove(p::environment::CONFIG_KEY);
    }
    let result: Config = serde_json::from_value(if value.is_null() { json!({}) } else { value })
        .map_err(|_| fault("InvalidInput", "invalid notes configuration"))?;
    if !(128..=16384).contains(&result.max_output_tokens)
        || !(1..=300_000).contains(&result.timeout_ms)
        || !(1024..=4_194_304).contains(&result.max_input_bytes)
    {
        return Err(fault("InvalidInput", "notes budget is out of range"));
    }
    Ok(result)
}
fn text(value: String) -> Item {
    Item::Message {
        role: "user".into(),
        content: vec![Block::Text { text: value }],
    }
}
fn extract(reply: ModelReply) -> Result<(String, Value), Fault> {
    if reply
        .items
        .iter()
        .any(|item| matches!(item, Item::ToolCall { .. }))
    {
        return Err(fault(
            "InvalidCheckpoint",
            "notes generation attempted a tool call",
        ));
    }
    let result: String = reply
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Message { role, content } if role == "assistant" => Some(content),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if result.trim().is_empty() {
        return Err(fault(
            "InvalidCheckpoint",
            "notes generation returned no text",
        ));
    }
    Ok((result, reply.usage))
}
async fn prepare(
    request: c::Request,
    cx: CallContext,
    config: Config,
) -> Result<Option<c::Plan>, Fault> {
    if request.reason == c::Reason::BranchSummary {
        return Err(fault(
            "Unsupported",
            "notes does not carry a sibling branch into this branch; navigate without summary or \
             select summary policy",
        ));
    }
    if request.max_cut == 0 {
        return Ok(None);
    }
    // Exactly one bounded inference attempt. A hard budget refusal keeps the old projection.
    let serialized = serde_json::to_string(&request.projected)
        .map_err(|_| fault("InvalidInput", "cannot encode notes input"))?;
    if serialized.len() > config.max_input_bytes {
        return Err(fault(
            "ContextOverflow",
            "notes preparation exceeds its input budget; no checkpoint committed",
        ));
    }
    let reply: ModelReply = cx
        .call(
            p::auxiliary::PROVIDER,
            &p::auxiliary::Request::Generate {
                purpose: "notes".into(),
                timeout_ms: config.timeout_ms,
                input: ModelInput {
                    target: request.input.target,
                    max_output_tokens: Some(config.max_output_tokens),
                    tools: vec![],
                    items: vec![
                        Item::Message {
                            role: "system".into(),
                            content: vec![Block::Text {
                                text: "Maintain concise durable notes for the next context \
                                       window. Preserve goals, constraints, decisions, progress, \
                                       exact rare details, pending work and uncertain tool \
                                       effects. Update prior notes. Never execute or propose \
                                       replaying historical tools. Original details remain \
                                       available through history_recall; do not invent record \
                                       identities. Return only the updated notes."
                                    .into(),
                            }],
                        },
                        text(serialized),
                        text(request.input.instructions),
                    ],
                },
            },
        )
        .await?;
    let (notes, usage) = extract(reply)?;
    let path = p::history::active_path(&request.input.records)?;
    let source_ids = path
        .iter()
        .take(request.max_cut)
        .map(|r| r.sequence)
        .collect();
    Ok(Some(c::Plan {
        cut: request.max_cut,
        summary: notes.clone(),
        usage,
        states: vec![ExtensionState {
            namespace: NAMESPACE.into(),
            version: 1,
            required: true,
            summary: notes.clone(),
            references: source_ids,
            value: json!({ "text": notes }),
        }],
    }))
}
fn interpret(request: InterpretRequest) -> Result<InterpretReply, Fault> {
    for state in request.states.iter().filter(|state| state.required) {
        if state.namespace != NAMESPACE
            || state.version != 1
            || state.value["text"]
                .as_str()
                .is_none_or(|text| text.trim().is_empty())
        {
            return Err(fault(
                "MissingInterpreter",
                "unsupported required notes state or version; use explicit migration",
            ));
        }
    }
    Ok(InterpretReply {
        items: vec![text(
            "Durable notes restored for this branch. Use history_recall to search or read \
             original details on demand; results include record identities and continuation \
             cursors. Historical tools have not been replayed."
                .into(),
        )],
    })
}
fn migrate(request: MigrateRequest) -> Result<MigrateReply, Fault> {
    let mut states = request.states;
    let mut losses = vec![];
    for state in &mut states {
        if state.namespace != NAMESPACE {
            if state.required {
                return Err(fault(
                    "Unsupported",
                    "notes migrator cannot translate another required namespace",
                ));
            }
            continue;
        }
        if state.version != 1 {
            losses.push(format!(
                "{} v{} opaque state is replaced by its public summary",
                state.namespace, state.version
            ));
            if state.summary.trim().is_empty() {
                return Err(fault(
                    "InvalidCheckpoint",
                    "cannot migrate an empty public notes summary",
                ));
            }
            state.version = 1;
            state.value = json!({ "text": state.summary });
        }
    }
    Ok(MigrateReply {
        states,
        preserved: vec![
            "original history, public notes summaries and declared source references".into(),
        ],
        losses,
    })
}
fn description() -> p::configuration::Description {
    p::configuration::Description {
        schema: Some(json!({
            "type": "object",
            "properties": {
                "max_output_tokens": { "type": "integer", "minimum": 128, "maximum": 16384 },
                "timeout_ms": { "type": "integer", "minimum": 1, "maximum": 300000 },
                "max_input_bytes": { "type": "integer", "minimum": 1024, "maximum": 4194304 },
            },
            "additionalProperties": false,
        })),
        defaults: json!({
            "max_output_tokens": 2048,
            "timeout_ms": 60000,
            "max_input_bytes": 262144,
        }),
        description: Some(
            "One bounded notes preparation; changes rebuild only this instance. Select the notes \
             policy and interpreter explicitly."
                .into(),
        ),
        ..Default::default()
    }
}
fn descriptor() -> p::Descriptor {
    p::Descriptor {
        package: "notes".into(),
        version: "0.1.0".into(),
        provides: vec![
            c::POLICY.into(),
            INTERPRETER.into(),
            MIGRATOR.into(),
            p::configuration::CONFIGURATION.into(),
        ],
    }
}
fn create(value: Value) -> Result<Package, Fault> {
    let settings = config(value)?;
    Ok(Package::new("notes")
        .service(c::POLICY, move |request, cx| {
            prepare(request, cx, settings.clone())
        })
        .service(INTERPRETER, |request, _| async move { interpret(request) })
        .service(MIGRATOR, |request, _| async move { migrate(request) })
        .service(
            p::configuration::CONFIGURATION,
            |request: p::configuration::PluginRequest, _| async move {
                match request {
                    p::configuration::PluginRequest::Describe => Ok(json!(description())),
                    p::configuration::PluginRequest::Validate { config } => {
                        Ok(json!(p::configuration::validate(&description(), &config)?))
                    }
                    p::configuration::PluginRequest::Update { .. } => Err(fault(
                        "Unsupported",
                        "notes configuration requires local rebuild",
                    )),
                }
            },
        ))
}
eden_plugin_sdk::export_plugin!(descriptor, create);

#[cfg(test)]
mod tests {
    use super::*;
    fn state(version: u32) -> ExtensionState {
        ExtensionState {
            namespace: NAMESPACE.into(),
            version,
            required: true,
            summary: "saved notes".into(),
            value: json!({ "text": "saved notes" }),
            references: vec![1],
        }
    }
    #[test]
    fn empty_generation_and_tool_calls_cannot_become_checkpoint() {
        for items in [
            vec![],
            vec![Item::ToolCall {
                call_id: "old".into(),
                name: "write".into(),
                arguments: "{}".into(),
            }],
        ] {
            assert!(
                extract(ModelReply {
                    items,
                    usage: Value::Null
                })
                .is_err()
            );
        }
    }
    #[test]
    fn unknown_required_version_needs_explicit_loss_preview() {
        assert!(
            interpret(InterpretRequest {
                states: vec![state(2)]
            })
            .is_err()
        );
        let preview = migrate(MigrateRequest {
            states: vec![state(2)],
            apply: false,
        })
        .unwrap();
        let applied = migrate(MigrateRequest {
            states: vec![state(2)],
            apply: true,
        })
        .unwrap();
        assert_eq!(json!(preview), json!(applied));
        assert_eq!(preview.losses.len(), 1);
        assert_eq!(preview.states[0].references, vec![1]);
        interpret(InterpretRequest {
            states: preview.states,
        })
        .unwrap();
    }
    #[test]
    fn configuration_limits_and_descriptor_are_real() {
        assert!(config(json!({ "max_output_tokens": 0 })).is_err());
        assert_eq!(create(Value::Null).unwrap().descriptor(), &descriptor());
    }
}
