//! Resource identity and expansion stay with the host; completion follows its loaded revision.
use crate::{Adapter, fault};
use eden_protocol::{Fault, coding::Block, resources::Snapshot};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

// Keep templates callable when their bare name is owned by a pager command or alias.
const RESERVED: &[&str] = &[
    "resume",
    "rename",
    "auth",
    "login",
    "logout",
    "config",
    "sessions",
    "resources",
    "settings",
    "model",
    "effort",
    "compact",
    "copy",
    "find",
    "expand",
    "jump",
    "edit-prompt",
    "multiline",
    "compact-mode",
    "timestamps",
    "toggle-mouse-reporting",
    "theme",
    "vim-mode",
    "exit",
    "quit",
    "help",
    "eden-status",
    "usage",
    "capabilities",
    "voice",
    "loop",
    "tasks",
    "imagine",
    "imagine-video",
    "hooks-add",
    "hooks-list",
    "hooks-remove",
    "hooks-trust",
    "hooks-untrust",
    "reload-plugins",
];

fn commands(snapshot: &Snapshot) -> Vec<Value> {
    let mut commands = vec![
        json!({ "name": "eden-status", "description": "Inspect the actual Eden host state" }),
        json!({ "name": "usage", "description": "Show reported Eden token usage" }),
        json!({
            "name": "capabilities",
            "description": "Connected capabilities and remaining work",
        }),
    ];
    for skill in &snapshot.skills {
        commands.push(json!({
            "name": format!("skill:{}", skill.name),
            "description": skill.description,
            "input": { "hint": "arguments" },
            "_meta": { "scope": "user", "path": skill.path },
        }));
    }
    for template in &snapshot.templates {
        commands.push(json!({
            "name": format!("template:{}", template.name),
            "description": template.description,
            "input": { "hint": "template arguments (quotes supported)" },
        }));
        if !RESERVED
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&template.name))
        {
            commands.push(json!({
                "name": template.name,
                "description": template.description,
                "input": { "hint": "template arguments (quotes supported)" },
            }));
        }
    }
    commands
}

impl Adapter {
    pub(crate) async fn resource_inventory(&self) -> Result<Snapshot, Fault> {
        if self.view.lock().await.snapshot.state.read_only {
            return Ok(Snapshot::default());
        }
        self.client.resources().await
    }

    pub(crate) async fn available_commands(&self) -> Result<Vec<Value>, Fault> {
        Ok(commands(&self.resource_inventory().await?))
    }

    pub(crate) fn publish_resources(&self, snapshot: &Snapshot, loading: bool) {
        let previous = self
            .resource_revision
            .fetch_max(snapshot.revision, Ordering::AcqRel);
        // A loaded history inherits the pager's bootstrap commands. Even revision zero
        // must replace them, so a read-only view cannot offer the initiating host's skills.
        if loading || previous < snapshot.revision {
            self.update(
                json!({
                    "sessionUpdate": "available_commands_update",
                    "availableCommands": commands(snapshot),
                }),
                false,
            );
        }
    }

    pub(crate) async fn resource_content(
        &self,
        body: &str,
        blocks: &[Value],
    ) -> Result<Vec<Block>, Fault> {
        let command = body.split_whitespace().next().unwrap_or("");
        let mut canonical = None;
        if body.starts_with('/') {
            let inventory = self.resource_inventory().await?;
            let name = command.trim_start_matches('/');
            if let Some(template) = name.strip_prefix("template:") {
                if !inventory
                    .templates
                    .iter()
                    .any(|resource| resource.name == template)
                {
                    return Err(fault(format!(
                        "Unknown template: {template}. Inspect /resources or reload from disk."
                    )));
                }
                canonical = Some(format!("/{template}"));
            } else if name.starts_with("skill:") {
                // The host returns the owning source's unknown-skill diagnostic before model I/O.
            } else if RESERVED
                .iter()
                .any(|reserved| reserved.eq_ignore_ascii_case(name))
                || !inventory
                    .templates
                    .iter()
                    .any(|template| template.name == name)
            {
                return Err(fault(
                    "This command is not connected; no model request was sent. Inspect /resources \
                     for skills and templates.",
                ));
            }
        }
        blocks
            .iter()
            .enumerate()
            .map(|(index, block)| {
                if block["type"] != "text" {
                    return Err(fault("This attachment type is not connected yet"));
                }
                let mut text = block["text"].as_str().unwrap_or_default().to_owned();
                if index == 0
                    && let Some(canonical) = &canonical
                {
                    text = format!(
                        "{canonical}{}",
                        text.strip_prefix(command).ok_or_else(|| fault(
                            "Template invocation must begin in the first text block"
                        ))?
                    );
                }
                Ok(Block::Text { text })
            })
            .collect()
    }

    pub(crate) async fn resource_description(&self, snapshot: &Snapshot) -> Result<String, Fault> {
        let (cwd, read_only) = {
            let view = self.view.lock().await;
            (
                view.snapshot.state.cwd.clone(),
                view.snapshot.state.read_only,
            )
        };
        let mut result = format!("Cwd: {cwd}\nLoaded revision: {}\n", snapshot.revision);
        if read_only {
            result.push_str(
                "Read-only history: live resource discovery and reload are unavailable.\n",
            );
        } else {
            let trust = self.post("/trust/inspect", json!({})).await?;
            result.push_str(&format!(
                "Project trusted: {} (startup override: {})\n",
                trust["trusted"], trust["startup_override"]
            ));
            result.push_str(
                "Reload keeps this Session's cwd and startup trust. Reopen with an explicit trust \
                 choice to change it.\n",
            );
            for diagnostic in trust["diagnostics"].as_array().into_iter().flatten() {
                result.push_str(&format!("{diagnostic}\n"));
            }
        }
        result.push_str("\nSkills: /skill:<name> [arguments]\n");
        for resource in &snapshot.skills {
            result.push_str(&format!(
                "/skill:{} — {}\n  {} · model invocable: {}\n",
                resource.name, resource.description, resource.path, resource.model_invocable
            ));
        }
        result.push_str("\nTemplates: /template:<name> [arguments]; bare /<name> when unclaimed\n");
        for resource in &snapshot.templates {
            result.push_str(&format!(
                "/template:{} — {}\n  {}\n",
                resource.name, resource.description, resource.path
            ));
        }
        result.push_str("\nInstruction sources:\n");
        for source in &snapshot.sources {
            result.push_str(&format!("{source}\n"));
        }
        for diagnostic in &snapshot.diagnostics {
            result.push_str(&format!("{:?}: {}\n", diagnostic.level, diagnostic.message));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_keep_a_qualified_entry_when_a_pager_command_owns_the_bare_name() {
        let mut snapshot = Snapshot::default();
        for name in ["model", "voice"] {
            snapshot.templates.push(eden_protocol::resources::Resource {
                name: name.into(),
                description: "template".into(),
                path: "model.md".into(),
                model_invocable: false,
            });
        }
        let commands = commands(&snapshot);
        assert!(
            commands
                .iter()
                .any(|command| command["name"] == "template:model")
        );
        assert!(!commands.iter().any(|command| command["name"] == "model"));
        assert!(
            commands
                .iter()
                .any(|command| command["name"] == "template:voice")
        );
        assert!(!commands.iter().any(|command| command["name"] == "voice"));
    }
}
