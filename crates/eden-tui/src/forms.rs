//! Convert authoritative presentation fields without inferring a second configuration schema.
use crate::model::Field;
use eden_protocol::{
    configuration_form::{Binding, Control, Edit},
    presentation::{LiveView, Node},
};
use serde_json::{Value, json};
#[derive(Clone)]
pub struct Target {
    pub owner: String,
    pub view: String,
    pub revision: u64,
    pub node: String,
    pub action: String,
    pub binding: Option<Binding>,
}
pub fn find(view: &LiveView, node: &str) -> Option<(Target, Vec<Field>)> {
    fn find_node<'a>(nodes: &'a [Node], id: &str) -> Option<&'a Node> {
        for n in nodes {
            if n.id() == id {
                return Some(n);
            }
            if let Node::Group { children, .. } = n
                && let Some(found) = find_node(children, id)
            {
                return Some(found);
            }
        }
        None
    }
    let node = find_node(&view.view.nodes, node)?;
    let mut target = Target {
        owner: view.owner.clone(),
        view: view.view.id.clone(),
        revision: view.revision,
        node: node.id().into(),
        action: String::new(),
        binding: None,
    };
    let fields = match node {
        Node::ConfigurationForm {
            binding, fields, ..
        } => {
            target.binding = Some(binding.clone());
            fields
                .iter()
                .map(|f| {
                    let value = f.value.clone().unwrap_or(Value::Null);
                    let mut field = Field::text(
                        &f.label,
                        if f.control == Control::Secret {
                            ""
                        } else {
                            value.as_str().unwrap_or("")
                        },
                    );
                    field.key = f.path.clone();
                    field.kind = serde_json::to_value(&f.control)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default();
                    field.private = f.control == Control::Secret;
                    field.readonly = !f.writable;
                    field.allow_clear = true;
                    if !matches!(f.control, Control::Text | Control::Secret) {
                        field.value = value.to_string();
                    }
                    field.option_values = f.options.clone();
                    field.options = f
                        .options
                        .iter()
                        .map(|v| {
                            v.as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| v.to_string())
                        })
                        .collect();
                    if f.control == Control::Choice {
                        field.value = value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string());
                    }
                    field.initial = field.value.clone();
                    field.cursor = field.value.len();
                    field
                })
                .collect()
        }
        Node::Form { action, fields, .. } => {
            target.action = action.clone();
            fields
                .iter()
                .map(|f| {
                    let mut field = Field::text(
                        &f.label,
                        f.initial.as_ref().and_then(Value::as_str).unwrap_or(""),
                    );
                    field.key = f.id.clone();
                    field.required = f.required;
                    field.options = f.options.clone();
                    field.kind = serde_json::to_value(&f.kind)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default();
                    if let Some(v) = &f.initial
                        && !v.is_string()
                    {
                        field.value = v.to_string();
                    }
                    field.initial = field.value.clone();
                    field.cursor = field.value.len();
                    field
                })
                .collect()
        }
        _ => return None,
    };
    Some((target, fields))
}
pub fn entries(nodes: &[Node], out: &mut Vec<(String, String)>) {
    for node in nodes {
        match node {
            Node::Form { id, .. } | Node::ConfigurationForm { id, .. } => {
                out.push((id.clone(), format!("Edit {id}")))
            }
            Node::Button { id, label, .. } => out.push((id.clone(), label.clone())),
            Node::Group { children, .. } => entries(children, out),
            _ => {}
        }
    }
}
pub fn submission(
    target: &Target,
    fields: &[Field],
    session: u64,
    id: &str,
    action: &str,
) -> Result<(Value, Vec<Edit>), String> {
    crate::fields::validate(fields).map_err(|(_, e)| e)?;
    let mut private = Vec::new();
    let mut edits = Vec::new();
    let mut values = serde_json::Map::new();
    for f in fields {
        if f.readonly {
            continue;
        }
        if target.binding.is_some() {
            if f.value == f.initial && !f.clear && !f.inherit {
                continue;
            }
            let edit = if f.inherit {
                Edit::Inherit {
                    path: f.key.clone(),
                }
            } else if f.clear {
                Edit::Clear {
                    path: f.key.clone(),
                }
            } else {
                Edit::Set {
                    path: f.key.clone(),
                    value: f.parsed_value()?,
                }
            };
            if f.private {
                private.push(edit)
            } else {
                edits.push(edit)
            }
        } else {
            values.insert(f.key.clone(), f.parsed_value()?);
        }
    }
    let action = if target.binding.is_some() {
        format!("{}:{action}", target.node)
    } else {
        target.action.clone()
    };
    let values = if let Some(binding) = &target.binding {
        json!({ "binding": binding, "edits": edits })
    } else {
        Value::Object(values)
    };
    Ok((
        json!({
            "session_id": session,
            "owner": target.owner,
            "view_id": target.view,
            "revision": target.revision,
            "action": action,
            "request_id": id,
            "values": values,
        }),
        private,
    ))
}

pub fn content(nodes: &[Node], out: &mut Vec<String>) {
    for node in nodes {
        match node {
            Node::Text { text, .. } | Node::Status { text, .. } => out.push(text.clone()),
            Node::Code { language, text, .. } => out.push(format!(
                "```{}\n{text}\n```",
                language.as_deref().unwrap_or("")
            )),
            Node::Diff { before, after, .. } => {
                out.push(format!("Before\n{before}\nAfter\n{after}"))
            }
            Node::Table { columns, rows, .. } => {
                out.push(columns.join(" | "));
                for row in rows {
                    out.push(row.join(" | "));
                }
            }
            Node::Group {
                title, children, ..
            } => {
                out.push(title.clone());
                content(children, out)
            }
            Node::Attachment {
                name,
                record_sequence,
                ..
            } => out.push(format!("Attachment {name} · record {record_sequence}")),
            Node::Form { id, .. } | Node::ConfigurationForm { id, .. } => {
                out.push(format!("Form {id} · /live to edit"))
            }
            Node::Button { label, .. } => out.push(format!("[{label}] · /live")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target() -> Target {
        Target {
            owner: "config".into(),
            view: "settings".into(),
            revision: 1,
            node: "fields".into(),
            action: String::new(),
            binding: Some(Binding {
                instance: "one".into(),
                generation: Some(1),
                revision: 1,
                profile: 0,
            }),
        }
    }
    #[test]
    fn private_material_is_not_in_the_public_retry_envelope() {
        let mut field = Field::text("Token", "");
        field.key = "/token".into();
        field.private = true;
        field.value = "PRIVATE_CANARY".into();
        let (public, private) = submission(&target(), &[field], 7, "request", "apply").unwrap();
        assert!(!public.to_string().contains("PRIVATE_CANARY"));
        assert!(
            serde_json::to_string(&private)
                .unwrap()
                .contains("PRIVATE_CANARY")
        );
        let retry = json!({ "request": public, "inputs": [] });
        assert!(!retry.to_string().contains("PRIVATE_CANARY"));
    }
    #[test]
    fn numeric_and_json_edits_keep_types_and_explicit_null() {
        let mut integer = Field::integer("Count", 1, 0, 100);
        integer.key = "/count".into();
        integer.value = "12".into();
        let mut json_field = Field::text("Value", "{}");
        json_field.key = "/value".into();
        json_field.kind = "json".into();
        json_field.value = "null".into();
        let (request, _) =
            submission(&target(), &[integer, json_field], 7, "request", "preview").unwrap();
        assert_eq!(request["values"]["edits"][0]["value"], json!(12));
        assert!(request["values"]["edits"][1]["value"].is_null());
    }
}

#[cfg(test)]
mod inherit_edit_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    #[test]
    fn explicit_input_after_inherit_selects_set_or_clear() {
        let target = Target {
            owner: "config".into(),
            view: "settings".into(),
            revision: 1,
            node: "fields".into(),
            action: String::new(),
            binding: Some(Binding {
                instance: "one".into(),
                generation: Some(1),
                revision: 1,
                profile: 0,
            }),
        };
        let mut text = Field::text("value", "");
        text.key = "/value".into();
        text.inherit = true;
        text.paste("new").unwrap();
        let mut boolean = Field::boolean("flag", false);
        boolean.key = "/flag".into();
        boolean.inherit = true;
        boolean.activate();
        let mut choice = Field::choice("mode", "a", &["a", "b"]);
        choice.key = "/mode".into();
        choice.inherit = true;
        choice.choose(1);
        let mut clear = Field::text("clear", "old");
        clear.key = "/clear".into();
        clear.allow_clear = true;
        clear.inherit = true;
        clear
            .handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
            .unwrap();
        let (request, _) = submission(
            &target,
            &[text, boolean, choice, clear],
            7,
            "request",
            "preview",
        )
        .unwrap();
        let operations: Vec<_> = request["values"]["edits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|edit| edit["operation"].as_str().unwrap())
            .collect();
        assert_eq!(operations, ["set", "set", "set", "clear"]);
    }
}
