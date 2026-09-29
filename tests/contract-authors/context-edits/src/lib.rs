//! Independent policy author: only public SDK types cross the native boundary.
use eden_plugin_sdk::{
    Package,
    protocol::{
        Descriptor, Fault,
        coding::{Block, Item},
        context_edit::{Boundary, Entry, PolicyInput, PolicyOutput, Scope},
    },
    serde_json::{self, Value},
};
const ROLE: &str = "test.context-policy.v1";
fn fault(message: &str) -> Fault {
    Fault::new("AuthorPolicyFailure", "context-edits", message)
}
fn transform(mut input: PolicyInput) -> Result<PolicyOutput, Fault> {
    let expected = if input.config["expected_boundary"] == "run_finish" {
        Boundary::RunFinish
    } else {
        Boundary::BeforeRequest
    };
    if input.boundary != expected {
        return Err(fault("unexpected boundary"));
    }
    let scope: Scope = serde_json::from_value(input.config["scope"].clone())
        .map_err(|_| fault("scope configuration missing"))?;
    match input.config["operation"].as_str() {
        Some("first") => {
            input.document.entries.push(Entry {
                id: "inserted:author-system".into(),
                item: Item::Message {
                    role: "system".into(),
                    content: vec![Block::Text {
                        text: "POLICY-FIRST".into(),
                    }],
                },
                references: vec![],
            });
            input.document.tools.retain(|tool| tool.name == "bash");
            if input.document.tools.len() != 1 {
                return Err(fault("expected the actual bash declaration"));
            }
        }
        Some("second") => {
            if !input
                .protected
                .iter()
                .any(|id| id == "inserted:author-system")
                || input.document.tools.len() != 1
                || input.document.tools[0].name != "bash"
            {
                return Err(fault(
                    "second policy did not receive the first policy's document and protected \
                     identity",
                ));
            }
            let entry = input
                .document
                .entries
                .iter_mut()
                .find(|entry| entry.id == "inserted:author-system")
                .ok_or_else(|| fault("first policy missing"))?;
            if !serde_json::to_string(&entry.item)
                .map_err(|_| fault("cannot inspect first policy"))?
                .contains("POLICY-FIRST")
            {
                return Err(fault("first marker missing"));
            }
            entry.item = Item::Message {
                role: "system".into(),
                content: vec![Block::Text {
                    text: "POLICY-FIRST-SECOND".into(),
                }],
            };
            input.document.tools.clear();
        }
        Some("fail") => return Err(fault("intentional policy failure")),
        Some("invalid") => input.document.entries.push(Entry {
            id: "inserted:orphan-call".into(),
            item: Item::ToolCall {
                call_id: "orphan".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            },
            references: vec![],
        }),
        _ => return Err(fault("unknown policy operation")),
    }
    Ok(PolicyOutput {
        document: input.document,
        scope,
    })
}
fn descriptor() -> Descriptor {
    Descriptor {
        package: "context-edits".into(),
        version: "0.1.0".into(),
        provides: vec![ROLE.into()],
    }
}
fn create(_: Value) -> Result<Package, Fault> {
    Ok(
        Package::new("context-edits").service(ROLE, |input: PolicyInput, _| async move {
            transform(input)
        }),
    )
}
eden_plugin_sdk::export_plugin!(descriptor, create);
