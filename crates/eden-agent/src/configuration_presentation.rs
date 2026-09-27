//! Management presentation is an adapter over the existing configuration transaction authority.
use super::*;
use eden_plugin_sdk::configuration_form::{self as helper, Model};
use eden_protocol::{configuration_form as f, presentation as p};
use serde_json::{Value, json};

pub(crate) const OWNER: &str = "eden-host-configuration";
fn fault(code: &str, message: &str) -> Fault {
    Fault::new(code, "configuration", message)
}
impl Session {
    /// Obtain only public configuration inputs for either an author's form or the SDK helper.
    pub async fn configuration_form_model(&self, instance: &str) -> Result<Model, Fault> {
        let inspection = self.inspect_configuration().await?;
        let item = inspection
            .instances
            .into_iter()
            .find(|item| item.id == instance)
            .ok_or_else(|| fault("InvalidInput", "unknown configuration instance"))?;
        Ok(Model {
            binding: f::Binding {
                instance: item.id,
                generation: item.generation,
                revision: inspection.revision,
                profile: item.description.profile.unwrap_or(1),
            },
            description: item.description,
            effective: item.effective,
            sources: item.field_sources,
            secrets_configured: item.secrets_configured,
        })
    }
    /// Open a reusable settings view while idle. Authors optionally implement the public form service.
    /// Missing form services use the SDK helper; unavailable author services retain a host recovery form.
    pub async fn open_configuration(&self, instance: &str) -> Result<p::Revision, Fault> {
        let model = self.configuration_form_model(instance).await?;
        let kernel = self.0.kernel.get()?;
        let composition = kernel.composition();
        let spec = configuration::specs(&composition)
            .into_iter()
            .find(|s| s.id == instance)
            .ok_or_else(|| fault("InvalidInput", "unknown instance"))?;
        let custom = kernel.instance_running(instance)
            && composition.packages.iter().any(|m| {
                m.descriptor.package == spec.package
                    && m.descriptor.provides.iter().any(|c| c == helper::FORM)
            });
        let mut view = if custom {
            let reply = kernel
                .invoke_package_management(
                    instance,
                    Request {
                        execution: None,
                        session_id: self.id(),
                        run_id: 0,
                        contract: helper::FORM.into(),
                        payload: json!(model),
                    },
                    Cancellation::default(),
                )
                .await
                .into_result();
            match reply.and_then(|value| {
                serde_json::from_value(value)
                    .map_err(|_| fault("IncompatibleContract", "invalid author form"))
            }) {
                Ok(view) => view,
                Err(_) => helper::default_form(&model).node(p::Node::Status {
                    id: "form-fallback".into(),
                    text: "Author form unavailable; host configuration form".into(),
                }),
            }
        } else {
            helper::default_form(&model)
        };
        view.id = format!("configuration-{instance}");
        self.publish_configuration(view).await
    }
    /// Publish an author-constructed management view using exactly the same configuration authority.
    pub async fn publish_configuration(&self, mut view: p::View) -> Result<p::Revision, Fault> {
        fn bindings(nodes: &[p::Node], result: &mut Vec<f::Binding>) {
            for node in nodes {
                match node {
                    p::Node::ConfigurationForm { binding, .. } => result.push(binding.clone()),
                    p::Node::Group { children, .. } => bindings(children, result),
                    _ => {}
                }
            }
        }
        let mut targets = vec![];
        bindings(&view.nodes, &mut targets);
        if targets.is_empty() || view.source.is_some() {
            return Err(fault(
                "InvalidInput",
                "management view requires configuration bindings and no history source",
            ));
        }
        for binding in targets {
            let model = self.configuration_form_model(&binding.instance).await?;
            if model.binding != binding {
                return Err(fault("Conflict", "configuration binding changed"));
            }
            refresh_fields(&mut view.nodes, &model);
        }
        self.0.presentation.publish_management(OWNER, view)
    }
    /// Refresh all forms bound to an instance without replacing author layout or stable view IDs.
    async fn refresh_configuration_views(&self, instance: &str) -> Result<Vec<p::Revision>, Fault> {
        let model = self.configuration_form_model(instance).await?;
        let mut revisions = vec![];
        let canonical = format!("configuration-{instance}");
        let mut regenerate = false;
        for mut live in self.presentation_snapshot().views {
            if live.scope == p::ViewScope::Management
                && refresh_fields(&mut live.view.nodes, &model)
            {
                if live.view.id == canonical {
                    regenerate = true;
                } else {
                    revisions.push(self.0.presentation.publish_management(OWNER, live.view)?);
                }
            }
        }
        if regenerate {
            revisions.push(self.open_configuration(instance).await?);
        }
        Ok(revisions)
    }
    pub(crate) async fn configuration_presentation_action(
        &self,
        request: &p::ActionRequest,
    ) -> Result<Value, Fault> {
        let submission: f::Submission = serde_json::from_value(request.values.clone())
            .map_err(|_| fault("InvalidInput", "invalid configuration submission"))?;
        let action = request
            .action
            .rsplit_once(':')
            .map(|(_, action)| action)
            .ok_or_else(|| fault("InvalidInput", "invalid configuration action"))?;
        if action == "refresh" {
            return Ok(json!(
                self.refresh_configuration_views(&submission.binding.instance)
                    .await?
            ));
        }
        let current = self
            .configuration_form_model(&submission.binding.instance)
            .await?;
        if current.binding != submission.binding {
            return Err(fault(
                "Conflict",
                "configuration changed; refresh before submitting",
            ));
        }
        let change = configuration::Change {
            instance: submission.binding.instance.clone(),
            revision: submission.binding.revision,
            patch: json!({}),
            edits: submission.edits,
            replacement: None,
        };
        match action {
            "validate" => Ok(json!(self.validate_configuration(change).await?)),
            "preview" => Ok(json!(self.preview_configuration(change).await?)),
            "apply" | "cancel_apply" => {
                let mode = if action == "apply" {
                    configuration::ApplyMode::Wait
                } else {
                    configuration::ApplyMode::Cancel
                };
                let operation = self.apply_configuration(change, mode).await?;
                let receipt = self.wait_configuration(operation).await?;
                // Publication failure cannot hide a completed transaction or trigger another application.
                let _ = self
                    .refresh_configuration_views(&submission.binding.instance)
                    .await;
                Ok(json!(receipt))
            }
            _ => Err(fault("InvalidInput", "unknown configuration action")),
        }
    }
}

// Direct authors choose their own field decomposition, including entire public JSON subtrees.
fn refresh_fields(nodes: &mut [p::Node], model: &Model) -> bool {
    let mut changed = false;
    for node in nodes {
        match node {
            p::Node::ConfigurationForm {
                binding, fields, ..
            } if binding.instance == model.binding.instance => {
                *binding = model.binding.clone();
                for field in fields {
                    let path = &field.path;
                    let secret = model
                        .description
                        .secret_paths
                        .iter()
                        .any(|p| p == path || path.starts_with(&format!("{p}/")));
                    let contains_secret = model
                        .description
                        .secret_paths
                        .iter()
                        .any(|p| p.starts_with(&format!("{path}/")));
                    field.value = if secret || contains_secret {
                        None
                    } else {
                        model.effective.pointer(path).cloned()
                    };
                    field.source = model.sources.get(path).cloned();
                    field.configured = model.secrets_configured.get(path).copied().unwrap_or(false);
                    field.writable &= !secret
                        && !contains_secret
                        && model.binding.profile == 1
                        && (model.description.editable_layers.is_empty()
                            || model
                                .description
                                .editable_layers
                                .contains(&eden_protocol::configuration::Layer::Explicit));
                    if secret {
                        field.control = f::Control::Secret;
                    }
                    if secret || contains_secret {
                        field.options.clear();
                    }
                }
                changed = true;
            }
            p::Node::Group { children, .. } => changed |= refresh_fields(children, model),
            _ => {}
        }
    }
    changed
}
