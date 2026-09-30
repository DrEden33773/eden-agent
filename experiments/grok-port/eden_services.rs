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
