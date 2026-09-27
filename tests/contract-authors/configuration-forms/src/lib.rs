//! Independent native consumer of public configuration and portable form contracts.
use eden_plugin_sdk::{
    Package,
    configuration_form::{self as helper, Model},
    protocol::{self as p, Fault, configuration as config, presentation as view},
    serde_json::{Value, json},
};
use std::sync::{Arc, Mutex};
const PACKAGE: &str = "configuration-forms";
const STATE_SERVICE: &str = "author.configuration-forms.state.v1";
#[cfg(test)]
const INVALID_KEY: &str = "label";
#[cfg(test)]
const LIVE_KEY: &str = "label";
#[cfg(test)]
const RESTART_KEY: &str = "restart_tag";
struct State {
    initial: Value,
    effective: Value,
    updates: u64,
}
impl State {
    fn new(config: Value) -> Self {
        Self {
            initial: config.clone(),
            effective: config,
            updates: 0,
        }
    }
    fn handle(&mut self, request: config::PluginRequest) -> Result<Value, Fault> {
        match request {
            config::PluginRequest::Describe => Ok(json!(description())),
            config::PluginRequest::Validate { config } => Ok(json!(validate(&config)?)),
            config::PluginRequest::Update { config } => {
                let validation = validate(&config)?;
                if validation.errors.is_empty() {
                    self.effective = config;
                    self.updates += 1;
                }
                Ok(json!(validation))
            }
        }
    }
}
fn validate(candidate: &Value) -> Result<config::Validation, Fault> {
    let mut validation = config::validate(&description(), candidate)?;
    if candidate["label"] == "forbidden" {
        validation.errors.push(config::FieldError {
            path: "/label".into(),
            code: "reserved".into(),
            message: "This value is reserved by the author".into(),
        });
    }
    Ok(validation)
}
fn descriptor() -> p::Descriptor {
    p::Descriptor {
        package: PACKAGE.into(),
        version: "0.1.0".into(),
        provides: vec![
            config::CONFIGURATION.into(),
            helper::FORM.into(),
            STATE_SERVICE.into(),
        ],
    }
}
fn create(config: Value) -> Result<Package, Fault> {
    let validation = validate(&config)?;
    if !validation.errors.is_empty() {
        return Err(Fault::new(
            "InvalidInput",
            PACKAGE,
            "Configuration failed author validation",
        ));
    }
    let state = Arc::new(Mutex::new(State::new(config)));
    let update = state.clone();
    Ok(Package::new(PACKAGE)
        .service(
            config::CONFIGURATION,
            move |request: config::PluginRequest, _| {
                let state = update.clone();
                async move {
                    state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .handle(request)
                }
            },
        )
        .service(helper::FORM, |model: Model, _| async move {
            Ok::<_, Fault>(presentation(&model))
        })
        .service(STATE_SERVICE, move |_: Value, cx| {
            let state = state.clone();
            async move {
                let state = state.lock().unwrap_or_else(|e| e.into_inner());
                let secrets = description().secret_paths;
                Ok::<_, Fault>(json!({
                    "effective": config::redact(&state.effective, &secrets),
                    "initial": config::redact(&state.initial, &secrets),
                    "updates": state.updates,
                    "identity": cx.identity(),
                }))
            }
        }))
}
eden_plugin_sdk::export_plugin!(descriptor, create);

fn defaults() -> Value {
    json!({
        "label": "first",
        "enabled": true,
        "count": 2,
        "ratio": 0.5,
        "mode": "fast",
        "nested": { "path": "./notes" },
        "paths": ["one"],
        "extra": { "preserved": true },
        "restart_tag": "initial",
    })
}
fn description() -> config::Description {
    config::Description {
        schema: Some(json!({
            "type": "object",
            "properties": {
                "label": { "type": "string", "minLength": 1, "title": "Label" },
                "enabled": { "type": "boolean", "title": "Enabled" },
                "count": { "type": "integer", "minimum": 1, "maximum": 10, "title": "Count" },
                "ratio": { "type": "number", "minimum": 0, "maximum": 1, "title": "Ratio" },
                "mode": { "type": "string", "enum": ["fast", "careful"], "title": "Mode" },
                "nested": {
                    "type": "object",
                    "properties": { "path": { "type": "string", "title": "Storage path" } },
                },
                "paths": { "type": "array", "items": { "type": "string" }, "title": "Paths" },
                "extra": { "title": "Advanced JSON" },
                "token": { "type": "string", "title": "Private token" },
                "restart_tag": { "type": "string", "title": "Restart tag" },
            },
        })),
        defaults: defaults(),
        secret_paths: vec!["/token".into()],
        live_paths: [
            "label", "enabled", "count", "ratio", "mode", "nested", "paths", "extra",
        ]
        .into_iter()
        .map(|p| format!("/{p}"))
        .collect(),
        description: Some("Independent default-helper author".into()),
        ..config::Description::default()
    }
}
fn presentation(model: &Model) -> view::View {
    helper::default_form(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_update_preserves_the_effective_state() {
        let initial = defaults();
        let mut state = State::new(initial.clone());
        let mut bad = initial.clone();
        bad[INVALID_KEY] = json!("forbidden");
        let result = state
            .handle(config::PluginRequest::Update { config: bad })
            .unwrap();
        assert!(!result["errors"].as_array().unwrap().is_empty());
        assert_eq!(state.effective, initial);
        assert_eq!(state.updates, 0);
    }
    #[test]
    fn declared_live_changes_are_distinct_from_restart_changes() {
        let initial = defaults();
        let mut live = initial.clone();
        live[LIVE_KEY] = json!("changed");
        assert!(config::is_live_change(&description(), &initial, &live));
        let mut restart = initial.clone();
        restart[RESTART_KEY] = json!("changed");
        assert!(!config::is_live_change(&description(), &initial, &restart));
    }
}

#[cfg(test)]
mod form_tests {
    use super::*;
    fn form_node(
        nodes: &[view::Node],
    ) -> (
        &p::configuration_form::Binding,
        &[p::configuration_form::Field],
    ) {
        for node in nodes {
            match node {
                view::Node::ConfigurationForm {
                    binding, fields, ..
                } => return (binding, fields),
                view::Node::Group { children, .. } => return form_node(children),
                _ => {}
            }
        }
        panic!("author must expose a configuration form")
    }
    #[test]
    fn author_form_preserves_host_binding_and_field_paths() {
        let mut effective = defaults();
        for style in ["direct", "helper"] {
            effective["style"] = json!(style);
            let model = Model {
                binding: p::configuration_form::Binding {
                    instance: "fixture".into(),
                    generation: Some(19),
                    revision: 42,
                    profile: 2,
                },
                description: description(),
                effective: effective.clone(),
                sources: Default::default(),
                secrets_configured: Default::default(),
            };
            let view = presentation(&model);
            let (binding, fields) = form_node(&view.nodes);
            assert_eq!(binding, &model.binding);
            assert!(
                fields
                    .iter()
                    .any(|field| field.path == format!("/{LIVE_KEY}"))
            );
            assert!(
                fields
                    .iter()
                    .any(|field| field.path == format!("/{RESTART_KEY}"))
            );
        }
    }
    #[test]
    fn repeated_live_updates_are_observable_without_changing_initial_configuration() {
        let initial = defaults();
        let mut state = State::new(initial.clone());
        for value in ["second", "third"] {
            let mut candidate = initial.clone();
            candidate[LIVE_KEY] = json!(value);
            assert_eq!(
                state
                    .handle(config::PluginRequest::Update {
                        config: candidate.clone()
                    })
                    .unwrap(),
                json!({ "errors": [] })
            );
            assert_eq!(state.effective, candidate);
        }
        assert_eq!(state.initial, initial);
        assert_eq!(state.updates, 2);
    }
}
