// Modified by Eden Agent for native host integration; see the accompanying EDEN-FRONTEND.md.
//! Eden owns business services; the pager retains its original local widgets.
use crate::settings::{SettingCategory, SettingMeta, SettingValue};
use xai_acp_lib::{AcpAgentTx, acp_send};

pub(crate) fn enabled() -> bool {
    #[cfg(test)]
    if TEST_ENABLED.with(|enabled| enabled.get()) {
        return true;
    }
    cfg!(not(test))
}

pub(crate) fn command_connected(name: &str) -> bool {
    matches!(
        name,
        "new"
            | "clear"
            | "resume"
            | "rename"
            | "auth"
            | "login"
            | "logout"
            | "config"
            | "sessions"
            | "resources"
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

pub(crate) fn context_update(
    notification: &agent_client_protocol::ExtNotification,
    app: &mut crate::app::app_view::AppView,
) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(notification.params.get()) else {
        return false;
    };
    let Some(session) = value["sessionId"].as_str() else {
        return false;
    };
    for agent in app.agents.values_mut() {
        if agent
            .session
            .session_id
            .as_ref()
            .is_some_and(|id| id.0.as_ref() == session)
        {
            agent.eden_context = value.clone();
        }
    }
    true
}

pub(crate) fn context_line(
    value: &serde_json::Value,
    model_window: Option<u64>,
    width: u16,
    theme: &crate::theme::Theme,
) -> ratatui::text::Line<'static> {
    use crate::views::context_bar::{blend_color, default_breakpoints, fmt_tokens};
    let used = value["used"].as_u64();
    let total = value["window"].as_u64().or(model_window).filter(|n| *n > 0);
    let mut label = format!(
        "{}{} / {}",
        if used.is_some() && value["estimated"] == true {
            "~"
        } else {
            ""
        },
        used.map(fmt_tokens).unwrap_or_else(|| "?".into()),
        total.map(fmt_tokens).unwrap_or_else(|| "?".into())
    );
    let ratio = used.zip(total).map(|(u, t)| u as f64 / t as f64);
    let cells = if width >= 70 {
        10
    } else if width >= 42 {
        5
    } else {
        0
    };
    if let Some(ratio) = ratio.filter(|_| cells > 0) {
        let filled = (ratio.clamp(0.0, 1.0) * cells as f64).round() as usize;
        label.push_str(&format!(
            "  {}{}",
            "█".repeat(filled),
            "░".repeat(cells - filled)
        ));
    }
    let color = ratio
        .map(|r| blend_color(r * 100.0, &default_breakpoints(theme)))
        .unwrap_or(theme.text_primary);
    ratatui::text::Line::from(ratatui::text::Span::styled(
        label,
        ratatui::style::Style::default().fg(color).bg(theme.bg_base),
    ))
}

static CONTEXT_VISIBLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub(crate) fn context_visible() -> bool {
    CONTEXT_VISIBLE.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn set_context_visible(value: bool) {
    CONTEXT_VISIBLE.store(value, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn title_update(
    notification: &agent_client_protocol::ExtNotification,
    app: &mut crate::app::app_view::AppView,
) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(notification.params.get()) else {
        return false;
    };
    let (Some(session), Some(title)) = (value["sessionId"].as_str(), value["title"].as_str())
    else {
        return false;
    };
    for agent in app.agents.values_mut() {
        if agent
            .session
            .session_id
            .as_ref()
            .is_some_and(|id| id.0.as_ref() == session)
        {
            agent.display_name =
                Some(crate::views::session_title::sanitize_display_text(title).into_owned());
        }
    }
    true
}

#[cfg(test)]
mod eden_context_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_is_not_zero_and_narrow_counts_survive() {
        let theme = crate::theme::Theme::default();
        assert_eq!(
            context_line(&json!({}), None, 38, &theme).to_string(),
            "? / ?"
        );
        assert_eq!(
            context_line(
                &json!({ "used": 18200, "estimated": true }),
                Some(128000),
                38,
                &theme
            )
            .to_string(),
            "~18K / 128K"
        );
    }

    #[test]
    fn high_occupancy_uses_error_color_and_full_character_cells() {
        let theme = crate::theme::Theme::default();
        let line = context_line(
            &json!({ "used": 99000, "window": 100000, "estimated": true }),
            None,
            100,
            &theme,
        );
        assert!(line.to_string().contains("██████████"));
        assert_eq!(line.spans[0].style.fg, Some(theme.accent_error));
    }
}

#[cfg(test)]
thread_local! { static TEST_ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(test)]
pub(crate) struct TestEnabled;
#[cfg(test)]
pub(crate) fn test_enabled() -> TestEnabled {
    TEST_ENABLED.with(|enabled| {
        assert!(!enabled.replace(true));
    });
    TestEnabled
}
#[cfg(test)]
impl Drop for TestEnabled {
    fn drop(&mut self) {
        TEST_ENABLED.with(|enabled| enabled.set(false));
    }
}

/// Progressive lists use the same request generation and dispatch gate as final lists.
pub(crate) fn directory_progress(
    notification: &agent_client_protocol::ExtNotification,
    app: &mut crate::app::app_view::AppView,
) -> bool {
    use crate::app::actions::{Action, TaskResult};
    use crate::views::session_picker_surface::SessionPickerHost;
    let Ok(value) = serde_json::from_str::<serde_json::Value>(notification.params.get()) else {
        return false;
    };
    let picker = &value["picker"];
    let host = match picker["host"].as_str() {
        Some("AgentModal") => SessionPickerHost::AgentModal,
        Some("Welcome") => SessionPickerHost::Welcome,
        Some("Dashboard") => SessionPickerHost::Dashboard,
        _ => return false,
    };
    let Some(generation) = picker["generation"].as_u64() else {
        return false;
    };
    let Some(seq) = picker["seq"].as_u64() else {
        return false;
    };
    let query = picker["query"].as_str().map(str::to_owned);
    let Ok(sessions) = crate::app::effects::session_list::parse_session_picker_entries_with(
        value,
        crate::app::effects::session_list::LocalPresence::Relabel,
        |_| Ok(Default::default()),
    ) else {
        return false;
    };
    let effects = crate::app::dispatch::dispatch(
        Action::TaskComplete(TaskResult::SessionListLoaded {
            host,
            generation,
            sessions,
            partial: None,
            scope: xai_grok_shell::session::unified_list::ListScope::Cwd,
            seq,
            query,
        }),
        app,
    );
    app.pending_effects.extend(effects);
    true
}

#[derive(Default)]
struct FrameTrace {
    consumed: Vec<serde_json::Value>,
    queued: std::collections::VecDeque<(u64, Vec<serde_json::Value>)>,
}
thread_local! { static FRAME_TRACE: std::cell::RefCell<FrameTrace> = std::cell::RefCell::new(FrameTrace::default()); }
/// Observations are retained until the terminal writer acknowledges the frame that follows dispatch.
pub(crate) fn consumed(value: serde_json::Value) -> bool {
    if std::env::var_os("EDEN_FRONTEND_TRACE").is_none() {
        return false;
    }
    FRAME_TRACE.with(|trace| trace.borrow_mut().consumed.push(value));
    true
}
pub(crate) fn drawn(sequence: u64, app: &crate::app::app_view::AppView) {
    if std::env::var_os("EDEN_FRONTEND_TRACE").is_none() {
        return;
    }
    FRAME_TRACE.with(|trace| {
        let mut trace = trace.borrow_mut();
        let mut receipts = std::mem::take(&mut trace.consumed);
        let active = match app.active_view {
            crate::app::app_view::ActiveView::Agent(id) => app.agents.get(&id),
            _ => None,
        };
        for receipt in &mut receipts {
            receipt["view"] = serde_json::json!(
                active
                    .and_then(|agent| agent.session.session_id.as_ref())
                    .map(|id| id.0.as_ref())
            );
            receipt["ready"] =
                serde_json::json!(active.is_some_and(|agent| !agent.session.loading_replay));
        }
        if !receipts.is_empty() {
            trace.queued.push_back((sequence, receipts));
        }
    });
}
pub(crate) fn written(sequence: u64) {
    use std::io::Write;
    let Some(path) = std::env::var_os("EDEN_FRONTEND_TRACE") else {
        return;
    };
    FRAME_TRACE.with(|trace| {
        let mut trace = trace.borrow_mut();
        while trace
            .queued
            .front()
            .is_some_and(|(target, _)| *target <= sequence)
        {
            let (_, receipts) = trace.queued.pop_front().unwrap();
            for mut receipt in receipts {
                receipt["direction"] = serde_json::json!("frame");
                receipt["time_ns"] = serde_json::json!(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_nanos())
                        .unwrap_or_default()
                );
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let mut line = receipt.to_string().into_bytes();
                    line.push(b'\n');
                    let _ = file.write_all(&line);
                }
            }
        }
    });
}
