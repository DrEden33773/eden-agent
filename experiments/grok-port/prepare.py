#!/usr/bin/env python3
"""Apply the Eden transport seam to an extracted, fixed Grok source archive."""

import argparse
import shutil
from pathlib import Path

HERE = Path(__file__).resolve().parent

MODIFIED = (
    "lib.rs",
    "acp/mod.rs",
    "acp/eden_transport.rs",
    "eden_services.rs",
    "app/cli.rs",
    "app/session_startup.rs",
    "slash/commands/mod.rs",
    "slash/commands/model.rs",
    "settings/registry.rs",
    "settings/defs.rs",
    "actions/mod.rs",
    "views/modal.rs",
    "app/effects/mod.rs",
    "app/effects/session_list.rs",
    "app/dispatch/session/foreign.rs",
    "app/dispatch/session/load.rs",
    "app/dispatch/settings/setters.rs",
    "app/dispatch/task_result.rs",
    "views/settings_modal/state.rs",
    "views/settings_modal/render.rs",
    "views/agent.rs",
    "app/agent_view/input.rs",
    "views/settings_modal/input.rs",
    "app/dispatch/prompt.rs",
)
NOTICE = "// Modified by Eden Agent for native host integration; see the accompanying EDEN-FRONTEND.md.\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    args = parser.parse_args()
    root = args.source
    patch_services(root)
    patch_management(root)
    patch_model_feedback(root)
    patch_model_failure_feedback(root)
    patch_connected_hints(root)
    patch_eden_cancel(root)
    patch_cancel_help(root)
    patch_busy_send(root)
    acp = root / "crates/codegen/xai-grok-pager/src/acp"
    path = acp / "mod.rs"
    source = path.read_text()
    marker = "pub async fn connect(cancel: &CancellationToken, flags: ConnectFlags) -> Result<AcpConnection> {"
    replacement = (
        marker
        + '\n    if let Some(adapter) = std::env::var_os("EDEN_GROK_BRIDGE") {\n        return eden_transport::connect(cancel, flags, adapter.into()).await;\n    }'
    )
    if "mod eden_transport;" not in source:
        assert source.count(marker) == 1
        source = source.replace(marker, replacement)
        source += "\nmod eden_transport;\n"
        path.write_text(source)
    shutil.copyfile(HERE / "eden_transport.rs", acp / "eden_transport.rs")

    startup = root / "crates/codegen/xai-grok-pager/src/app/session_startup.rs"
    source = startup.read_text()
    marker = ") -> anyhow::Result<ResolvedExisting> {"
    if "// Eden owns session persistence." not in source:
        assert source.count(marker) >= 1
        source = source.replace(
            marker,
            marker
            + '\n    // Eden owns session persistence.\n    if std::env::var_os("EDEN_GROK_BRIDGE").is_some() {\n        return Ok(ResolvedExisting { id: session_id.into(), original_cwd: None, title: None, deferred_local_miss: false, suppress_code_restore: true });\n    }',
            1,
        )
        startup.write_text(source)
    cli = root / "crates/codegen/xai-grok-pager/src/app/cli.rs"
    source = cli.read_text()
    marker = "pub fn pin_local_resume_target(&mut self) -> anyhow::Result<()> {"
    if "// Eden endpoint identity is checked by the adapter." not in source:
        source = source.replace(
            marker,
            marker
            + '\n        // Eden endpoint identity is checked by the adapter.\n        if std::env::var_os("EDEN_GROK_BRIDGE").is_some() { return Ok(()); }',
            1,
        )
        cli.write_text(source)

    for name in MODIFIED:
        path = root / "crates/codegen/xai-grok-pager/src" / name
        body = path.read_text()
        if not body.startswith(NOTICE):
            path.write_text(NOTICE + body)


def patch_services(root):
    pager = root / "crates/codegen/xai-grok-pager/src"
    shutil.copyfile(HERE / "eden_services.rs", pager / "eden_services.rs")
    path = pager / "lib.rs"
    source = path.read_text()
    if "mod eden_services;" not in source:
        path.write_text(source + "\nmod eden_services;\n")
    path = pager / "slash/commands/mod.rs"
    source = path.read_text()
    if "eden_services::command_connected" not in source:
        before, tests = source.split("#[cfg(test)]", 1)
        before = before.replace(
            "    vec![", "    let commands: Vec<Arc<dyn SlashCommand>> = vec![", 1
        )
        before = before.replace(
            "\n    ]\n}",
            "\n    ];\n    if crate::eden_services::enabled() { commands.into_iter().filter(|command| crate::eden_services::command_connected(command.name())).collect() } else { commands }\n}",
            1,
        )
        path.write_text(before + "#[cfg(test)]" + tests)
    path = pager / "settings/registry.rs"
    source = path.read_text()
    if "eden_services::setting_connected" not in source:
        source = source.replace(
            "let entries = crate::settings::defs::default_settings();",
            "let mut entries = crate::settings::defs::default_settings();\n        if crate::eden_services::enabled() { entries.retain(crate::eden_services::setting_connected); }",
        )
        path.write_text(source)
    path = pager / "slash/commands/model.rs"
    source = path.read_text()
    if "eden_services::enabled" not in source:
        source = source.replace(
            "return CommandResult::Action(Action::SetDefaultModel(id));",
            "return CommandResult::Action(if crate::eden_services::enabled() { Action::SwitchModel(ModelChoice::new(id)) } else { Action::SetDefaultModel(id) });",
        )
        path.write_text(source)
    path = pager / "app/effects/mod.rs"
    source = path.read_text()
    if "eden_services::persist" not in source:
        source = source.replace("mod helpers;", "pub(crate) mod helpers;", 1)
        source = source.replace(
            "Effect::PersistSetting { key, value, rollback_value } => {",
            "Effect::PersistSetting { key, value, rollback_value } => {\n            let tx = acp_tx.clone();",
            1,
        )
        source = source.replace(
            "match persist_setting(key, value.clone()).await {",
            "match if crate::eden_services::enabled() { crate::eden_services::persist(key, value.clone(), &tx).await } else { persist_setting(key, value.clone()).await } {",
            1,
        )
        path.write_text(source)


def patch_management(root):
    pager = root / "crates/codegen/xai-grok-pager/src"
    path = pager / "actions/mod.rs"
    source = path.read_text()
    if "eden_services::action_connected" not in source:
        source = source.replace(
            "Self::new(defaults::default_actions(\n            screen_mode,\n            mouse_reporting_toggle_enabled,\n        ))",
            "{ let mut actions = defaults::default_actions(screen_mode, mouse_reporting_toggle_enabled); if crate::eden_services::enabled() { actions.retain(|action| crate::eden_services::action_connected(action.id)); } Self::new(actions) }",
        )
        path.write_text(source)
    path = pager / "views/modal.rs"
    source = path.read_text()
    if "eden_services::palette_connected" not in source:
        source = source.replace(
            "    entries.retain(|entry| {",
            "    entries.retain(|entry| {\n        if crate::eden_services::enabled() && !crate::eden_services::palette_connected(entry) { return false; }",
            1,
        )
        path.write_text(source)
    path = pager / "settings/registry.rs"
    source = path.read_text()
    if "Eden selects explicit defaults" not in source:
        source = source.replace(
            "            out\n        }",
            "            // Eden selects explicit defaults; its API has no null target.\n            if crate::eden_services::enabled() { out.retain(|choice| !choice.canonical.is_empty()); }\n            out\n        }",
            1,
        )
        path.write_text(source)
    path = pager / "settings/defs.rs"
    source = path.read_text()
    if '"Use & save model"' not in source:
        source = source.replace(
            'label: "Default model",',
            'label: if crate::eden_services::enabled() { "Use & save model" } else { "Default model" },',
            1,
        )
        path.write_text(source)
    path = pager / "app/dispatch/settings/setters.rs"
    source = path.read_text()
    marker = "pub(in crate::app::dispatch) fn set_default_model(\n    app: &mut AppView,\n    new_id: acp::ModelId,\n) -> Vec<Effect> {"
    if "Eden applies the two scopes sequentially" not in source:
        assert marker in source
        source = source.replace(
            marker,
            marker
            + "\n    // Eden applies the two scopes sequentially and broadcasts the actual model.\n    if crate::eden_services::enabled() { return vec![Effect::PersistPreferredModel { model_id: new_id, reasoning_effort: None }]; }",
            1,
        )
        path.write_text(source)
    path = pager / "app/effects/mod.rs"
    source = path.read_text()
    if "Eden default operation" not in source and "Explicit Eden default writes" not in source:
        source = source.replace(
            "Effect::PersistPreferredModel { model_id, reasoning_effort } => {",
            "Effect::PersistPreferredModel { model_id, reasoning_effort } => {\n            // Eden default operation owns both scopes.\n            let tx = acp_tx.clone();",
            1,
        )
        source = source.replace(
            "let result = xai_grok_shell::util::config::persist_models_default(",
            'let result = if crate::eden_services::enabled() { crate::eden_services::persist("default_model", crate::settings::SettingValue::String(model_id_str), &tx).await } else { xai_grok_shell::util::config::persist_models_default(',
            1,
        )
        start = source.index("// Eden default operation")
        before, after = source[:start], source[start:]
        after = after.replace(".map_err(|e| e.to_string());", ".map_err(|e| e.to_string()) };", 1)
        path.write_text(before + after)
    path = pager / "app/dispatch/task_result.rs"
    source = path.read_text()
    if "Eden default result" not in source:
        marker = "TaskResult::PreferredModelPersisted { result } => {"
        source = source.replace(
            marker,
            marker
            + '\n            // Eden default result reports partial scope failure without false rollback.\n            if crate::eden_services::enabled() { app.show_toast(&match result { Ok(()) => "Current model and new-session default saved".to_string(), Err(error) => error }); return vec![]; }',
            1,
        )
        path.write_text(source)
    path = pager / "app/effects/session_list.rs"
    source = path.read_text()
    if "Eden history timestamps" not in source:
        source = source.replace(
            "let parsed_updated: Option<chrono::DateTime<chrono::Utc>> = v",
            "// Eden history timestamps are unix seconds from the host.\n            let parsed_updated: Option<chrono::DateTime<chrono::Utc>> = v",
            1,
        )
        marker = 'v\n                .get("updatedAt")'
        source = source.replace(
            marker,
            'v.get("updatedAtUnix").and_then(Value::as_i64).and_then(|seconds| chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, 0)).or_else(|| v\n                .get("updatedAt")',
            1,
        )
        start = source.index("// Eden history timestamps")
        before, after = source[:start], source[start:]
        after = after.replace(
            ".and_then(|s| s.parse().ok());", ".and_then(|s| s.parse().ok()));", 1
        )
        source = before + after
        source = source.replace(
            "if !is_conversation && ts < cutoff {",
            "if !crate::eden_services::enabled() && !is_conversation && ts < cutoff {",
            1,
        )
        source = source.replace(
            "    match presence {",
            "    if crate::eden_services::enabled() { return Ok(parsed); }\n    match presence {",
            1,
        )
        path.write_text(source)
    path = pager / "app/dispatch/session/foreign.rs"
    source = path.read_text()
    if "Eden sources come from the host" not in source:
        source = source.replace(
            "let foreign_effect = if app.chat_mode {",
            "// Eden sources come from the host, without scanning other agents' private stores.\n    let foreign_effect = if app.chat_mode || crate::eden_services::enabled() {",
            1,
        )
        path.write_text(source)
    path = pager / "app/dispatch/session/load.rs"
    source = path.read_text()
    if "Eden loads via the selected host" not in source:
        source = source.replace(
            "    let local_cwd = app.cwd.to_string_lossy().to_string();",
            "    // Eden loads via the selected host, not the Grok history store.\n    if crate::eden_services::enabled() { return dispatch_load_session(app, session_id, None, false); }\n    let local_cwd = app.cwd.to_string_lossy().to_string();",
        )
        path.write_text(source)


def patch_model_feedback(root):
    pager = root / "crates/codegen/xai-grok-pager/src"
    path = pager / "app/actions.rs"
    source = path.read_text()
    if "EdenPersistModel" not in source:
        source = source.replace(
            "    PersistPreferredModel {",
            "    /// Keep the originating Eden session across asynchronous default writes.\n    EdenPersistModel { agent_id: AgentId, session_id: acp::SessionId, model_id: acp::ModelId },\n    PersistPreferredModel {",
            1,
        )
        source = source.replace(
            "    PreferredModelPersisted {",
            "    /// A result is applied only to the agent that initiated the operation.\n    EdenModelPersisted { agent_id: AgentId, result: Result<(), String> },\n    PreferredModelPersisted {",
            1,
        )
        path.write_text(source)
    path = pager / "app/dispatch/settings/setters.rs"
    source = path.read_text()
    old = "if crate::eden_services::enabled() { return vec![Effect::PersistPreferredModel { model_id: new_id, reasoning_effort: None }]; }"
    new = 'if crate::eden_services::enabled() { if let ActiveView::Agent(agent_id) = app.active_view { if let Some(agent) = app.agents.get_mut(&agent_id) { if let Some(session_id) = agent.session.session_id.clone() { if let Some(crate::views::modal::ActiveModal::Settings { state }) = agent.active_modal.as_mut() { state.eden_status = Some("Applying current model and default…".into()); } return vec![Effect::EdenPersistModel { agent_id, session_id, model_id: new_id }]; } } } return vec![]; }'
    if old in source:
        path.write_text(source.replace(old, new, 1))
    path = pager / "app/effects/mod.rs"
    source = path.read_text()
    if "Effect::EdenPersistModel" not in source:
        marker = "        Effect::PersistPreferredModel { model_id, reasoning_effort } => {"
        new = """        Effect::EdenPersistModel { agent_id, session_id, model_id } => {
            let tx = acp_tx.clone();
            tasks.spawn(async move {
                let request = acp::ExtRequest::new("eden/model/default", serde_json::value::to_raw_value(&serde_json::json!({ "sessionId": session_id, "modelId": model_id })).expect("serialize model selection").into());
                let result = acp_send(request, &tx).await.map(|_| ()).map_err(|error| error.to_string());
                TaskResult::EdenModelPersisted { agent_id, result }
            });
        }
"""
        source = source.replace(marker, new + marker, 1)
        # Incidental upstream preference effects must not write either Eden scope.
        source = source.replace(
            "// Eden default operation owns both scopes.",
            "// Explicit Eden default writes use EdenPersistModel.\n            if crate::eden_services::enabled() { return (false, meta); }",
        )
        path.write_text(source)
    path = pager / "app/dispatch/task_result.rs"
    source = path.read_text()
    if "TaskResult::EdenModelPersisted" not in source:
        marker = "        TaskResult::PreferredModelPersisted { result } => {"
        new = """        TaskResult::EdenModelPersisted { agent_id, result } => {
            refresh_open_settings_modals(app);
            let message = match result { Ok(()) => "Current model and new-session default saved".to_string(), Err(error) => error };
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                if let Some(crate::views::modal::ActiveModal::Settings { state }) = agent.active_modal.as_mut() { state.eden_status = Some(message.clone()); }
                agent.show_toast(&message);
            }
            vec![]
        }
"""
        path.write_text(source.replace(marker, new + marker, 1))
    path = pager / "views/settings_modal/state.rs"
    source = path.read_text()
    if "pub eden_status" not in source:
        source = source.replace(
            "    pub close_on_picker_exit: bool,",
            "    pub close_on_picker_exit: bool,\n    /// Session-owned operation feedback remains visible inside the modal.\n    pub eden_status: Option<String>,",
            1,
        )
        source = source.replace(
            "            close_on_picker_exit: false,",
            "            close_on_picker_exit: false,\n            eden_status: None,",
            1,
        )
        path.write_text(source)
    path = pager / "views/settings_modal/render.rs"
    source = path.read_text()
    if "state.eden_status" not in source:
        source = source.replace(
            "        render_docs_footer(buf, footer_area, &theme);",
            """        if crate::eden_services::enabled() {
            modal_window::render_centered_tip_footer(buf, footer_area, &theme, state.eden_status.as_deref().unwrap_or("Display settings are local · Model actions use the Eden host"));
        } else { render_docs_footer(buf, footer_area, &theme); }""",
            1,
        )
        path.write_text(source)
    path = pager / "settings/defs.rs"
    source = path.read_text()
    source = source.replace(
        'description: "Model used for new sessions. Changing this also switches the active session. Pick `(no override)` to clear.",',
        'description: if crate::eden_services::enabled() { "Use this model in the current session and save it as the default for new sessions. /model only changes the current session." } else { "Model used for new sessions. Changing this also switches the active session. Pick `(no override)` to clear." },',
        1,
    )
    path.write_text(source)


def patch_model_failure_feedback(root):
    path = root / "crates/codegen/xai-grok-pager/src/app/dispatch/task_result.rs"
    source = path.read_text()
    old = 'let message = match result { Ok(()) => "Current model and new-session default saved".to_string(), Err(error) => error };'
    new = 'let (message, detail) = match result { Ok(()) => ("Current model and new-session default saved".to_string(), None), Err(error) => (if error.contains("Current session changed") { "Current model changed; default was not saved.".into() } else { "Model operation failed; see the transcript.".into() }, Some(error)) };'
    if old in source:
        source = source.replace(old, new, 1)
        source = source.replace(
            "if let Some(agent) = app.agents.get_mut(&agent_id) {\n                if let Some(crate::views::modal::ActiveModal::Settings { state })",
            "if let Some(agent) = app.agents.get_mut(&agent_id) {\n                if let Some(detail) = detail { agent.scrollback.push_block(RenderBlock::system(detail)); }\n                if let Some(crate::views::modal::ActiveModal::Settings { state })",
            1,
        )
        path.write_text(source)


def patch_connected_hints(root):
    path = root / "crates/codegen/xai-grok-pager/src/views/agent.rs"
    source = path.read_text()
    marker = 'hints.push(HintItem::new(crate::key!(BackTab), "mode"));'
    replacement = 'if !crate::eden_services::enabled() { hints.push(HintItem::new(crate::key!(BackTab), "mode")); }'
    if replacement not in source:
        assert marker in source
        path.write_text(source.replace(marker, replacement, 1))


def patch_eden_cancel(root):
    path = root / "crates/codegen/xai-grok-pager/src/app/agent_view/input.rs"
    source = path.read_text()
    marker = "        if self.scrollback_drag_latched() {"
    if "Eden cancellation keeps an in-progress draft" not in source:
        replacement = (
            """        // Eden cancellation keeps an in-progress draft and precedes local input handling.
        if crate::eden_services::enabled()
            && self.stoppable_activity_running()
            && let Event::Key(key) = ev
            && key.kind != KeyEventKind::Release
            && key!('c', CONTROL).matches(key)
        {
            self.cancel_trigger_hint = Some(crate::app::actions::CancelTrigger::CtrlC);
            return InputOutcome::Action(Action::CancelTurn);
        }
"""
            + marker
        )
        assert marker in source
        path.write_text(source.replace(marker, replacement, 1))


def patch_cancel_help(root):
    path = root / "crates/codegen/xai-grok-pager/src/actions/mod.rs"
    source = path.read_text()
    if "Eden cancel help" not in source:
        marker = "pub fn new(actions: Vec<ActionDef>) -> Self {\n        Self { actions }"
        replacement = """pub fn new(mut actions: Vec<ActionDef>) -> Self {
        // Eden cancel help follows the actual run-first policy.
        if crate::eden_services::enabled() {
            actions.retain(|action| crate::eden_services::action_connected(action.id));
            for action in &mut actions {
                if action.id == ActionId::CancelTurn {
                    action.long_help = Some("Ctrl+C cancels the running Eden operation without clearing the draft. Wait for cleanup to finish before continuing. At idle the local input and quit bindings still apply.");
                }
            }
        }
        Self { actions }"""
        assert marker in source
        path.write_text(source.replace(marker, replacement, 1))
    path = root / "crates/codegen/xai-grok-pager/src/views/settings_modal/input.rs"
    source = path.read_text()
    if "This action takes an explicit model" not in source:
        marker = "                Some((key, _meta)) => SettingsKeyOutcome::Action(Action::OpenResetConfirm { key }),"
        replacement = (
            """                Some(("default_model", _)) if crate::eden_services::enabled() => {
                    state.eden_status = Some("This action takes an explicit model; choose one with Enter.".into());
                    SettingsKeyOutcome::Changed
                }
"""
            + marker
        )
        assert marker in source
        path.write_text(source.replace(marker, replacement, 1))


def patch_busy_send(root):
    path = root / "crates/codegen/xai-grok-pager/src/app/dispatch/prompt.rs"
    source = path.read_text()
    marker = "    if !runs_locally && refuse_if_load_failed(app, id) {"
    if "Eden keeps an ordinary busy draft" not in source:
        replacement = (
            """    // Eden keeps an ordinary busy draft until the shared queue workflow is connected.
    if crate::eden_services::enabled() && consume_input && !runs_locally {
        if let Some(agent) = app.agents.get_mut(&id) {
            if agent.prompt_input_mode == crate::app::agent_view::PromptInputMode::Normal && agent.stoppable_activity_running() {
                agent.show_toast("A run is active; your draft is kept. Wait or Ctrl+C to cancel.");
                return prelude;
            }
        }
    }
"""
            + marker
        )
        assert marker in source
        path.write_text(source.replace(marker, replacement, 1))


if __name__ == "__main__":
    main()
