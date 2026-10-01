// Modified by Eden Agent for native host integration; see the accompanying EDEN-FRONTEND.md.
//! Transient Eden forms retain the original modal chrome and editor without using prompt storage.
use crate::app::actions::{Action, Effect};
use crate::app::agent::AgentId;
use crate::app::app_view::{ActiveView, AppView, InputOutcome};
use crate::input::line_editor::LineEditor;
use crate::views::modal::ActiveModal;
use crate::views::modal_window::{self as mw, ModalWindowState};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Paragraph, Widget},
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

/// Neither action/effect diagnostics nor task-result diagnostics may reveal private form material.
pub struct PrivateValue(pub Value);
impl std::fmt::Debug for PrivateValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[private management payload]")
    }
}
struct Field {
    spec: Value,
    editor: LineEditor,
    edit: Option<&'static str>,
}
pub struct State {
    pub generation: u64,
    page: Value,
    fields: Vec<Field>,
    focus: usize,
    busy: bool,
    status: String,
    window: ModalWindowState,
    hits: Vec<(Rect, usize)>,
    description_scroll: u16,
}
impl State {
    fn loading(kind: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let generation = NEXT.fetch_add(1, Ordering::Relaxed);
        Self {
            generation,
            page: json!({
                "title": format!("Eden {kind}"),
                "formId": format!("eden-panel-{generation}"),
            }),
            fields: vec![],
            focus: 0,
            busy: true,
            status: "Loading…".into(),
            window: ModalWindowState::new(),
            hits: vec![],
            description_scroll: 0,
        }
    }
    fn receive(&mut self, page: Value) {
        let preserve = page["preserveEdits"] == true;
        let old = std::mem::take(&mut self.fields);
        self.fields = page["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|spec| {
                let mut editor = LineEditor::default();
                if spec["control"] != "secret" {
                    editor.set_text(spec["value"].as_str().map(str::to_owned).unwrap_or_else(
                        || {
                            if spec["value"].is_null() {
                                String::new()
                            } else {
                                spec["value"].to_string()
                            }
                        },
                    ));
                }
                let mut field = Field {
                    spec: spec.clone(),
                    editor,
                    edit: None,
                };
                if preserve
                    && let Some(previous) =
                        old.iter().find(|field| field.spec["path"] == spec["path"])
                {
                    field.editor = previous.editor.clone();
                    field.edit = previous.edit;
                }
                field
            })
            .collect();
        self.page = page;
        self.busy = false;
        self.status = "Tab / ↑↓ navigate · Enter activate · Esc close".into();
        self.focus = self.focus.min(self.count().saturating_sub(1));
        self.description_scroll = 0;
    }
    fn count(&self) -> usize {
        self.fields.len() + self.page["actions"].as_array().map_or(0, Vec::len)
    }
    fn request(&mut self, action: &str) -> Result<Action, String> {
        let mut inputs = json!({});
        let mut edits = vec![];
        let mut private = vec![];
        for field in &self.fields {
            let path = field.spec["path"].as_str().unwrap_or("");
            inputs[path] = if field.spec["control"] == "boolean" {
                serde_json::from_str(field.editor.text()).map_err(|_| format!("Invalid boolean for {path}"))?
            } else {
                json!(field.editor.text())
            };
            let Some(operation) = field.edit else {
                continue;
            };
            let value = if operation != "set" {
                Value::Null
            } else {
                match field.spec["control"].as_str().unwrap_or("text") {
                    "text" | "secret" => json!(field.editor.text()),
                    _ => serde_json::from_str(field.editor.text()).map_err(|_| {
                        format!(
                            "Invalid value for {}",
                            field.spec["label"].as_str().unwrap_or(path)
                        )
                    })?,
                }
            };
            let edit = json!({ "operation": operation, "path": path, "value": value });
            if field.spec["control"] == "secret" {
                private.push(edit);
            } else {
                edits.push(edit);
            }
        }
        self.busy = action != "close";
        self.status = if action == "close" {
            "Closing…"
        } else {
            "Working…"
        }
        .into();
        Ok(Action::EdenRequest {
            generation: self.generation,
            request: PrivateValue(json!({
                "formId": self.page["formId"],
                "revision": self.page["revision"],
                "action": action,
                "inputs": inputs,
                "edits": edits,
                "privateEdits": private,
            })),
        })
    }
    fn close(&mut self) -> (InputOutcome, bool) {
        // Closing is always available, including validation errors and in-flight work.
        let request = Action::EdenRequest {
            generation: self.generation,
            request: PrivateValue(json!({ "formId": self.page["formId"], "action": "close" })),
        };
        (InputOutcome::Action(request), true)
    }
    fn activate(&mut self) -> InputOutcome {
        if self.busy {
            return InputOutcome::Changed;
        }
        if self.focus < self.fields.len() {
            self.focus = (self.focus + 1).min(self.count().saturating_sub(1));
            return InputOutcome::Changed;
        }
        let action = self.page["actions"][self.focus - self.fields.len()]["id"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        if matches!(action.as_str(), "open_browser" | "copy_url") {
            self.busy = true;
            self.status = "Working…".into();
            return InputOutcome::Action(Action::EdenRequest {
                generation: self.generation,
                request: PrivateValue(
                    json!({ "localAction": action, "url": self.page["authUrl"] }),
                ),
            });
        }
        match self.request(&action) {
            Ok(action) => InputOutcome::Action(action),
            Err(error) => {
                self.status = error;
                InputOutcome::Changed
            }
        }
    }
    pub fn input(&mut self, event: &Event) -> (InputOutcome, bool) {
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Release {
                return (InputOutcome::Unchanged, false);
            }
            if key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                return self.close();
            }
            if self.busy {
                return (InputOutcome::Changed, false);
            }
            match key.code {
                KeyCode::Tab | KeyCode::Down => self.focus = (self.focus + 1) % self.count().max(1),
                KeyCode::BackTab | KeyCode::Up => {
                    self.focus = (self.focus + self.count().max(1) - 1) % self.count().max(1)
                }
                KeyCode::PageDown => {
                    self.description_scroll = self.description_scroll.saturating_add(4)
                }
                KeyCode::PageUp => {
                    self.description_scroll = self.description_scroll.saturating_sub(4)
                }
                KeyCode::Enter if key.kind != KeyEventKind::Repeat => {
                    return (self.activate(), false);
                }
                _ => {
                    if let Some(field) = self
                        .fields
                        .get_mut(self.focus)
                        .filter(|field| field.spec["writable"] != false)
                    {
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && matches!(key.code, KeyCode::Char('u' | 'r'))
                        {
                            field.edit = Some(if key.code == KeyCode::Char('u') {
                                "clear"
                            } else {
                                "inherit"
                            });
                            field.editor.reset();
                        } else if field.spec["control"] == "boolean"
                            && matches!(
                                key.code,
                                KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
                            )
                        {
                            field.editor.set_text(if field.editor.text() == "true" {
                                "false"
                            } else {
                                "true"
                            });
                            field.edit = Some("set");
                        } else if field.spec["control"] == "choice"
                            && matches!(
                                key.code,
                                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                            )
                        {
                            let options = field.spec["options"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default();
                            if !options.is_empty() {
                                let current = options
                                    .iter()
                                    .position(|value| {
                                        value.to_string() == field.editor.text()
                                            || value.as_str() == Some(field.editor.text())
                                    })
                                    .unwrap_or(0);
                                let next = if key.code == KeyCode::Left {
                                    (current + options.len() - 1) % options.len()
                                } else {
                                    (current + 1) % options.len()
                                };
                                field.editor.set_text(options[next].to_string());
                                field.edit = Some("set");
                            }
                        } else {
                            let before = field.editor.text().to_owned();
                            field.editor.handle_key(key);
                            if before != field.editor.text() {
                                field.edit = Some("set");
                            }
                        }
                    }
                }
            }
        } else if let Event::Paste(text) = event {
            if !self.busy
                && let Some(field) = self
                    .fields
                    .get_mut(self.focus)
                    .filter(|field| field.spec["writable"] != false)
            {
                field.editor.insert_paste(text);
                field.edit = Some("set");
            }
        } else if let Event::Mouse(mouse) = event {
            if mw::handle_modal_mouse(&mut self.window, mouse.kind, mouse.column, mouse.row)
                == mw::ModalWindowOutcome::CloseRequested
            {
                return self.close();
            }
            match mouse.kind {
                MouseEventKind::ScrollDown => {
                    self.focus = (self.focus + 1).min(self.count().saturating_sub(1))
                }
                MouseEventKind::ScrollUp => self.focus = self.focus.saturating_sub(1),
                MouseEventKind::Down(MouseButton::Left) if !self.busy => {
                    if let Some((_, index)) = self
                        .hits
                        .iter()
                        .find(|(rect, _)| rect.contains((mouse.column, mouse.row).into()))
                    {
                        self.focus = *index;
                        if self.focus >= self.fields.len() {
                            return (self.activate(), false);
                        }
                    }
                }
                _ => {}
            }
        }
        (InputOutcome::Changed, false)
    }
    pub fn render(&mut self, buf: &mut Buffer, area: Rect, theme: &crate::theme::Theme) {
        let title = visible(self.page["title"].as_str().unwrap_or("Eden")).replace('\n', " ");
        let config = mw::ModalWindowConfig {
            title: &title,
            tabs: None,
            shortcuts: &[],
            sizing: mw::ModalSizing {
                min_width: 24,
                max_width: 110,
                v_margin: 2,
                v_pad: 1,
                footer_lines: 2,
                ..Default::default()
            },
            fold_info: None,
        };
        let Some(inner) = mw::render_modal_window(buf, area, &mut self.window, &config, theme)
        else {
            return;
        };
        let body = inner.content;
        if body.height == 0 || body.width == 0 {
            return;
        }
        let description = visible(self.page["description"].as_str().unwrap_or(""));
        let lines = crate::render::wrapping::word_wrap_lines(
            description.lines().map(Line::from),
            body.width as usize,
        );
        let maximum = if self.fields.is_empty() && self.count() <= 3 {
            body.height.saturating_sub(self.count() as u16 + 2).max(1)
        } else {
            (body.height / 3).clamp(1, 6)
        };
        let height = maximum.min(lines.len().max(1) as u16);
        self.description_scroll = self
            .description_scroll
            .min(lines.len().saturating_sub(height as usize) as u16);
        let paragraph = Paragraph::new(lines).style(Style::default().fg(theme.text_secondary));
        paragraph
            .scroll((self.description_scroll, 0))
            .render(Rect { height, ..body }, buf);
        let available = body.height.saturating_sub(height + 1) as usize;
        let first = self.focus.saturating_sub(available.saturating_sub(1));
        self.hits.clear();
        for offset in 0..available {
            let index = first + offset;
            if index >= self.count() {
                break;
            }
            let rect = Rect {
                y: body.y + height + 1 + offset as u16,
                height: 1,
                ..body
            };
            let mut cursor = None;
            let label = if let Some(field) = self.fields.get(index) {
                let label = field.spec["label"].as_str().unwrap_or("");
                let mut value = match field.edit {
                    Some("clear") => "<clear override>".into(),
                    Some("inherit") => "<inherit>".into(),
                    _ if field.spec["control"] == "secret" => {
                        if field.editor.text().is_empty() {
                            if field.spec["configured"] == true {
                                "<configured; unchanged>".into()
                            } else {
                                "<private input>".into()
                            }
                        } else {
                            "•".repeat(field.editor.text().chars().count().min(24))
                        }
                    }
                    _ => field.editor.text().to_owned(),
                };
                let prefix = Line::from(format!("  {label}: ")).width();
                if index == self.focus
                    && field.spec["writable"] != false
                    && !matches!(field.edit, Some("clear" | "inherit"))
                    && rect.width as usize > prefix + 1
                {
                    let width = rect.width as usize - prefix - 1;
                    let viewport = field.editor.viewport(width);
                    if field.spec["control"] != "secret" {
                        value = field.editor.text()[viewport.visible_byte_range.clone()].to_owned();
                        cursor = Some(prefix + viewport.cursor_display_column);
                    } else if !field.editor.text().is_empty() {
                        let shown = &field.editor.text()[viewport.visible_byte_range.clone()];
                        value = "•".repeat(shown.chars().count().min(width));
                        let before = &field.editor.text()
                            [viewport.visible_byte_range.start..field.editor.cursor_byte()];
                        cursor = Some(prefix + before.chars().count().min(width));
                    }
                }
                format!(
                    "{label}: {value}{}",
                    if field.edit.is_some() { " *" } else { "" }
                )
            } else {
                format!(
                    "[ {} ]",
                    self.page["actions"][index - self.fields.len()]["label"]
                        .as_str()
                        .unwrap_or("")
                )
            };
            let label: String = label
                .chars()
                .filter(|character| !character.is_control())
                .collect();
            let style = if index == self.focus {
                Style::default()
                    .fg(theme.text_primary)
                    .bg(theme.bg_highlight)
            } else {
                Style::default().fg(theme.text_primary)
            };
            Line::styled(
                format!("{} {label}", if index == self.focus { "›" } else { " " }),
                style,
            )
            .render(rect, buf);
            if let Some(column) = cursor.filter(|column| *column < rect.width as usize) {
                buf[(rect.x + column as u16, rect.y)].set_style(
                    Style::default()
                        .fg(theme.bg_highlight)
                        .bg(theme.text_primary),
                );
            }
            self.hits.push((rect, index));
        }
        let detail = self
            .fields
            .get(self.focus)
            .map(|field| {
                format!(
                    "{} · source: {} · {}",
                    field.spec["path"].as_str().unwrap_or(""),
                    field.spec["source"].as_str().unwrap_or("unknown"),
                    field.spec["description"].as_str().unwrap_or("")
                )
            })
            .unwrap_or_else(|| "PgUp/PgDn scroll details".into());
        Paragraph::new(visible(&format!("{detail}\n{}", self.status)))
            .style(Style::default().fg(theme.text_secondary))
            .render(inner.footer, buf);
    }
}

fn visible(text: &str) -> String {
    text.chars()
        .filter(|character| *character == '\n' || !character.is_control())
        .collect()
}

pub fn open(app: &mut AppView, kind: &'static str) -> Vec<Effect> {
    let ActiveView::Agent(agent_id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };
    let state = State::loading(kind);
    let generation = state.generation;
    agent.active_modal = Some(ActiveModal::Eden {
        state: Box::new(state),
    });
    vec![Effect::EdenUi {
        agent_id,
        session_id,
        generation,
        request: PrivateValue(
            json!({ "open": kind, "formId": format!("eden-panel-{generation}") }),
        ),
    }]
}
pub fn request(app: &mut AppView, generation: u64, request: PrivateValue) -> Vec<Effect> {
    let ActiveView::Agent(agent_id) = app.active_view else {
        return vec![];
    };
    let Some(session_id) = app
        .agents
        .get(&agent_id)
        .and_then(|agent| agent.session.session_id.clone())
    else {
        return vec![];
    };
    vec![Effect::EdenUi {
        agent_id,
        session_id,
        generation,
        request,
    }]
}
pub fn receive(
    app: &mut AppView,
    agent_id: AgentId,
    generation: u64,
    result: Result<PrivateValue, String>,
) -> Option<Action> {
    let agent = app.agents.get_mut(&agent_id)?;
    let Some(ActiveModal::Eden { state }) = agent.active_modal.as_mut() else {
        return None;
    };
    if state.generation != generation {
        return None;
    }
    match result {
        Ok(PrivateValue(page)) => {
            if let Some(message) = page["localStatus"].as_str() {
                state.busy = false;
                state.status = message.into();
                return None;
            }
            if let Some(session) = page["loadSession"].as_str() {
                let session = session.to_owned();
                agent.active_modal = None;
                return Some(Action::LoadSession(session, None, false));
            }
            if page["closed"] == true {
                agent.active_modal = None;
            } else {
                state.receive(page);
            }
        }
        Err(error) => {
            state.busy = false;
            state.status = error;
        }
    }
    None
}

/// URL actions remain transient and do not use clipboard-to-file fallback or prompt effects.
pub async fn local_action(request: &Value) -> Option<PrivateValue> {
    let action = request["localAction"].as_str()?.to_owned();
    let url = request["url"].as_str().unwrap_or("").to_owned();
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Some(PrivateValue(
            json!({ "localStatus": "Invalid authorization URL" }),
        ));
    }
    let message = tokio::task::spawn_blocking(move || match action.as_str() {
        "open_browser" => {
            if crate::link_opener::open_url_if_safe(
                &url,
                crate::terminal::hyperlinks::SchemeFilter::Standard,
            ) {
                "Opened authorization page".to_owned()
            } else {
                "Browser unavailable; use Copy authorization URL".to_owned()
            }
        }
        "copy_url" => crate::clipboard::copy_text(&url).message.to_owned(),
        _ => "Unknown local form action".to_owned(),
    })
    .await
    .unwrap_or_else(|_| "Could not complete local form action".into());
    Some(PrivateValue(json!({ "localStatus": message })))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn authorization_actions_reject_non_web_urls() {
        let result = local_action(&json!({
            "localAction": "open_browser",
            "url": "file:///private",
        }))
        .await
        .unwrap();
        assert_eq!(result.0["localStatus"], "Invalid authorization URL");
    }
    #[test]
    fn eden_confirmation_controls_submit_boolean_values() {
        let mut state = State::loading("sessions");
        state.receive(json!({ "formId": "stop", "revision": 0, "fields": [{ "path": "confirmed", "control": "boolean", "value": false, "writable": true }], "actions": [] }));
        let Action::EdenRequest { request, .. } = state.request("stop").unwrap() else { panic!("expected request"); };
        assert_eq!(request.0["inputs"]["confirmed"], false);
        state.busy = false;
        state.input(&Event::Key(crossterm::event::KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
        let Action::EdenRequest { request, .. } = state.request("stop").unwrap() else { panic!("expected request"); };
        assert_eq!(request.0["inputs"]["confirmed"], true);
    }
    #[test]
    fn private_payload_diagnostics_are_redacted() {
        let value = PrivateValue(json!({ "inputs": { "input": "PRIVATE_CANARY" } }));
        assert!(!format!("{value:?}").contains("PRIVATE_CANARY"));
    }
    #[test]
    fn private_field_does_not_leave_the_modal_or_render_as_plaintext() {
        let mut state = State::loading("auth");
        state.receive(json!({
            "title": "Auth",
            "formId": "one",
            "revision": 0,
            "fields": [{
                "path": "input",
                "label": "API key",
                "control": "secret",
                "writable": true,
            }],
            "actions": [{ "id": "submit", "label": "Submit" }],
        }));
        state.input(&Event::Paste("PRIVATE_CANARY".into()));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 24));
        state.render(
            &mut buffer,
            Rect::new(0, 0, 80, 24),
            &crate::theme::Theme::current(),
        );
        let rendered: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(!rendered.contains("PRIVATE_CANARY"));
        assert!(rendered.contains("•••"));
        let (result, close) = state.close();
        assert!(close);
        assert!(!format!("{result:?}").contains("PRIVATE_CANARY"));
    }
}
