//! Separate material ownership from public action admission and the committed configuration journal.
use super::*;
use eden_protocol::{configuration as c, configuration_form as f, private_input as p};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;

fn fault(code: &str, message: &str) -> Fault {
    Fault::new(code, "private_input", message)
}

// Plugin errors are untrusted text, including their code/source fields; keep only known status codes.
pub(crate) fn public_fault(error: Fault) -> Fault {
    let code = match error.code.as_str() {
        "Cancelled" => "Cancelled",
        "CleanupFailure" => "CleanupFailure",
        "Conflict" => "Conflict",
        "Unavailable" => "Unavailable",
        "ValidationFailed" => "ValidationFailed",
        _ => "ConfigurationFailure",
    };
    fault(
        code,
        "Configuration operation failed; inspect its status and retry with corrected input",
    )
}

#[derive(Serialize, Deserialize)]
struct Stored {
    instance: String,
    inputs: Vec<f::Edit>,
}
fn directory(root: &Path, reference: &str) -> Result<PathBuf, Fault> {
    if reference.is_empty()
        || reference.len() > 32
        || !reference.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(fault("InvalidInput", "invalid private reference"));
    }
    Ok(root.join("private-input").join(reference))
}
pub(crate) fn read(root: &Path, reference: &str, instance: &str) -> Result<Vec<f::Edit>, Fault> {
    let path = directory(root, reference)?.join("value.json");
    if !path.is_file() {
        return Err(fault(
            "PrivateInputUnavailable",
            "saved private input is unavailable in this installation",
        ));
    }
    let file = eden_workspace::private_file::open(&path)?;
    let stored: Stored = serde_json::from_reader(file).map_err(|_| {
        fault(
            "PrivateInputUnavailable",
            "saved private input cannot be read",
        )
    })?;
    if stored.instance != instance {
        return Err(fault(
            "InvalidInput",
            "private input belongs to a different receiver",
        ));
    }
    Ok(stored.inputs)
}
fn store(root: &Path, instance: String, inputs: Vec<f::Edit>) -> Result<String, Fault> {
    let parent = root.join("private-input");
    std::fs::create_dir_all(&parent)
        .map_err(|_| fault("Unavailable", "private storage is unavailable"))?;
    // Exclusive directory creation provides a fresh immutable identity without reusing any secret bytes.
    let (reference, directory) = loop {
        let reference = format!("{:x}", new_session_id());
        let path = directory(root, &reference)?;
        match std::fs::create_dir(&path) {
            Ok(()) => break (reference, path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(fault("Unavailable", "cannot create private storage")),
        }
    };
    let outcome = (|| {
        let mut file = eden_workspace::private_file::open(&directory.join("value.json"))?;
        let bytes = serde_json::to_vec(&Stored { instance, inputs })
            .map_err(|_| fault("InvalidInput", "invalid private input"))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| fault("Unavailable", "cannot save private input"))
    })();
    if let Err(error) = outcome {
        let _ = std::fs::remove_dir_all(directory);
        return Err(error);
    }
    Ok(reference)
}
pub(crate) fn validate_paths(
    description: &c::Description,
    inputs: &[f::Edit],
) -> Result<(), Fault> {
    let mut seen = std::collections::BTreeSet::new();
    for input in inputs {
        if !seen.insert(input.path())
            || !description
                .secret_paths
                .iter()
                .any(|path| path == input.path())
            || matches!(input, f::Edit::Inherit { .. })
            || input
                .path()
                .starts_with(&format!("/{}", eden_protocol::environment::CONFIG_KEY))
        {
            return Err(fault(
                "InvalidInput",
                "private input must set or clear a declared secret field",
            ));
        }
    }
    Ok(())
}
impl Session {
    /// Deliver material to a revision-bound configuration receiver shared by default and external plugins.
    /// Only `request` enters action admission. Once accepted, the host retains ownership on disconnect;
    /// retry that exact request with empty inputs to obtain the original result without retaining secrets.
    pub async fn submit_private_input(&self, input: p::Submission) -> Result<Value, Fault> {
        let p::Submission { request, inputs } = input;
        if request.owner != configuration_presentation::OWNER {
            return Err(fault("InvalidInput", "unknown private input receiver"));
        }
        let mut admission = self
            .0
            .presentation
            .admit_action(self.id(), &request, || Ok(None::<()>))?;
        if let Some((run, _)) = admission.take_dispatch() {
            let session = self.clone();
            tokio::spawn(async move {
                let result = if session.0.presentation.action_is_current(&request, run) {
                    session
                        .receive_private_configuration(&request, inputs)
                        .await
                } else {
                    Err(fault("Unavailable", "private input view changed"))
                };
                session.0.presentation.complete_action(request, result);
            });
        }
        admission.result().await
    }
    async fn receive_private_configuration(
        &self,
        request: &eden_protocol::presentation::ActionRequest,
        inputs: Vec<f::Edit>,
    ) -> Result<Value, Fault> {
        if inputs.is_empty() {
            return Err(fault(
                "PrivateInputRequired",
                "request was not accepted previously; enter private values again",
            ));
        }
        let submission: f::Submission = serde_json::from_value(request.values.clone())
            .map_err(|_| fault("InvalidInput", "invalid public configuration envelope"))?;
        let model = self
            .configuration_form_model(&submission.binding.instance)
            .await?;
        if model.binding != submission.binding {
            return Err(fault(
                "Conflict",
                "configuration changed; refresh before entering private values",
            ));
        }
        validate_paths(&model.description, &inputs)?;
        let snapshot = self.presentation_snapshot();
        let view = snapshot
            .views
            .iter()
            .find(|view| view.owner == request.owner && view.view.id == request.view_id)
            .ok_or_else(|| fault("Unavailable", "private input view is unavailable"))?;
        fn fields(nodes: &[eden_protocol::presentation::Node], id: &str) -> Option<Vec<f::Field>> {
            for node in nodes {
                match node {
                    eden_protocol::presentation::Node::ConfigurationForm {
                        id: node_id,
                        fields,
                        ..
                    } if node_id == id => return Some(fields.clone()),
                    eden_protocol::presentation::Node::Group { children, .. } => {
                        if let Some(found) = fields(children, id) {
                            return Some(found);
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        let (node, action) = request
            .action
            .rsplit_once(':')
            .ok_or_else(|| fault("InvalidInput", "invalid private action"))?;
        if !["validate", "preview", "apply", "cancel_apply"].contains(&action) {
            return Err(fault("InvalidInput", "private action is not supported"));
        }
        let offered = fields(&view.view.nodes, node)
            .ok_or_else(|| fault("InvalidInput", "private input form missing"))?;
        if inputs.iter().any(|input| {
            !offered.iter().any(|field| {
                field.path == input.path() && field.writable && field.control == f::Control::Secret
            })
        }) {
            return Err(fault("InvalidInput", "private input field is not writable"));
        }
        let reference = store(
            &self.0.workspace_options.global_dir,
            model.binding.instance,
            inputs,
        )?;
        let result = self
            .configuration_private_action(request, Some(reference.clone()))
            .await;
        if !result
            .as_ref()
            .is_ok_and(|value| value["status"] == "applied")
            && let Ok(path) = directory(&self.0.workspace_options.global_dir, &reference)
        {
            let _ = std::fs::remove_dir_all(path);
        }
        // All value-bearing receiver failures cross a static diagnostic boundary.
        result.map_err(public_fault)
    }
}

// Only descriptor-owned paths can appear in public diagnostics; private object member names cannot.
pub(crate) fn public_error_path(description: &c::Description, path: &str) -> String {
    for root in &description.secret_paths {
        if path == root || path.starts_with(&format!("{root}/")) {
            return root.clone();
        }
    }
    if description.secret_paths.is_empty() {
        return path.into();
    }
    let mut schema = description.schema.as_ref().unwrap_or(&Value::Null);
    for segment in path.strip_prefix('/').unwrap_or(path).split('/') {
        let name = segment.replace("~1", "/").replace("~0", "~");
        let Some(child) = schema
            .get("properties")
            .and_then(|properties| properties.get(&name))
        else {
            return String::new();
        };
        schema = child;
    }
    path.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_descendant_names_never_become_public_error_paths() {
        let description = c::Description {
            secret_paths: vec!["/token".into()],
            ..Default::default()
        };
        assert_eq!(public_error_path(&description, "/token/CANARY"), "/token");
        assert_eq!(public_error_path(&description, "/CANARY"), "");
    }
}

pub(crate) fn origins(inputs: &[f::Edit]) -> BTreeMap<String, String> {
    inputs
        .iter()
        .map(|edit| {
            (
                edit.path().to_owned(),
                match edit {
                    f::Edit::Clear { .. } => "explicit_session_clear",
                    _ => "explicit_session",
                }
                .to_owned(),
            )
        })
        .collect()
}
