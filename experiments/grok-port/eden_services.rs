//! Eden owns business services; the pager retains its original local widgets.
use crate::settings::{SettingCategory, SettingMeta, SettingValue};
use xai_acp_lib::{AcpAgentTx, acp_send};

pub(crate) fn enabled() -> bool {
    std::env::var_os("EDEN_GROK_BRIDGE").is_some()
}

pub(crate) fn command_connected(name: &str) -> bool {
    matches!(
        name,
        "resume"
            | "rename"
            | "auth"
            | "login"
            | "logout"
            | "config"
            | "sessions"
            | "settings"
            | "model"
            | "effort"
            | "compact"
            | "copy"
            | "find"
            | "expand"
            | "jump"
            | "edit-prompt"
            | "multiline"
            | "compact-mode"
            | "timestamps"
            | "toggle-mouse-reporting"
            | "theme"
            | "vim-mode"
            | "exit"
            | "quit"
            | "help"
    )
}

pub(crate) fn setting_connected(setting: &SettingMeta) -> bool {
    setting.key == "default_model"
        || (matches!(
            setting.category,
            SettingCategory::Appearance | SettingCategory::Mouse | SettingCategory::Editor
        ) && !matches!(
            setting.key,
            "screen_mode"
                | "dashboard_preview"
                | "combine_queued_prompts"
                | "follow_up_behavior"
                | "confirm_before_rewind"
                | "prompt_suggestions"
        ))
}

pub(crate) async fn persist(
    key: &'static str,
    value: SettingValue,
    tx: &AcpAgentTx,
) -> Result<(), String> {
    if key != "default_model" {
        return crate::app::effects::helpers::persist_setting(key, value).await;
    }
    let SettingValue::String(model_id) = value else {
        return Err("Expected an Eden model identity".into());
    };
    if model_id.is_empty() {
        return Err(
            "Select an explicit Eden model; clearing the default is not connected yet".into(),
        );
    }
    let request = agent_client_protocol::ExtRequest::new(
        "eden/model/default",
        serde_json::value::to_raw_value(&serde_json::json!({ "modelId": model_id }))
            .map_err(|error| error.to_string())?
            .into(),
    );
    acp_send(request, tx)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub(crate) fn action_connected(id: crate::actions::ActionId) -> bool {
    use crate::actions::ActionId;
    !matches!(
        id,
        ActionId::EnableVoiceMode
            | ActionId::VoiceToggle
            | ActionId::ToggleTodos
            | ActionId::ToggleTasks
            | ActionId::ToggleQueue
            | ActionId::OpenExtensions
            | ActionId::SendToBackground
            | ActionId::CycleMode
            | ActionId::ToggleYolo
            | ActionId::InterjectPrompt
            | ActionId::Rewind
            | ActionId::KillBgTask
            | ActionId::NewSession
            | ActionId::NewSessionInWorktree
            | ActionId::ExitSession
            | ActionId::OpenDashboard
    )
}

pub(crate) fn palette_connected(entry: &crate::views::modal::PaletteEntry) -> bool {
    use crate::views::modal::PaletteCommand;
    match &entry.command {
        PaletteCommand::SlashCommand(command) => command_connected(
            command
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or(""),
        ),
        PaletteCommand::Quit
        | PaletteCommand::KeyboardShortcuts
        | PaletteCommand::OpenSettings
        | PaletteCommand::SectionHeader(_)
        | PaletteCommand::EditPromptExternal => true,
        _ => false,
    }
}

/// The host broadcasts the complete selection so sparse compatibility mapping survives reopen.
pub(crate) fn model_update(
    notification: &agent_client_protocol::ExtNotification,
    app: &mut crate::app::app_view::AppView,
) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(notification.params.get()) else {
        return false;
    };
    let Some(session_id) = value["sessionId"].as_str() else {
        return false;
    };
    let Ok(models) =
        serde_json::from_value::<agent_client_protocol::SessionModelState>(value["models"].clone())
    else {
        return false;
    };
    for agent in app.agents.values_mut() {
        if agent
            .session
            .session_id
            .as_ref()
            .is_some_and(|id| id.0.as_ref() == session_id)
        {
            agent.session.models = Some(models.clone()).into();
            agent.session.models.model_changed_during_switch = agent.session.model_switch_pending;
            agent.refresh_context_total();
        }
    }
    true
}

pub(crate) fn thinking_label(
    models: &crate::acp::model_state::ModelState,
    name: &str,
) -> Option<String> {
    let info = models.available.get(models.current.as_ref()?)?;
    let thinking = info.meta.as_ref()?.get("edenThinking")?;
    let requested = thinking["requested"].as_str()?;
    let effective = thinking["effective"].as_str().unwrap_or("default");
    Some(if requested == effective {
        format!("{name} ({requested})")
    } else {
        format!("{name} ({requested} → {effective})")
    })
}
