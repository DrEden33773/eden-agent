//! Minimal keyboard consumer of the shared semantic presentation vocabulary.
use crate::live::call;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use eden_protocol::presentation::{
    ActionRequest, ActivityTarget, Field, FieldKind, LiveView, Node, Snapshot,
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, Stdout},
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type DraftKey = (String, String, String, String);
#[derive(Clone)]
enum Focus {
    Composer,
    Field {
        owner: String,
        view_id: String,
        node_id: String,
        action: String,
        field: Field,
        revision: u64,
    },
    Button {
        owner: String,
        view_id: String,
        action: String,
        revision: u64,
    },
}
struct Screen {
    snapshot: Snapshot,
    active_run: Option<u64>,
    composer: String,
    drafts: BTreeMap<DraftKey, Value>,
    focus: usize,
    message: String,
    request_number: u64,
    last_activity: Option<(ActivityTarget, Instant)>,
}
impl Screen {
    fn focusables(&self) -> Vec<Focus> {
        let mut items = vec![Focus::Composer];
        for view in &self.snapshot.views {
            if view.active {
                collect_focus(&view.view.nodes, view, &mut items);
            }
        }
        items
    }
    fn current(&self) -> Focus {
        let items = self.focusables();
        items[self.focus.min(items.len() - 1)].clone()
    }
    fn request_id(&mut self) -> String {
        self.request_number += 1;
        format!(
            "tui-{}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            self.request_number
        )
    }
}
fn collect_focus(nodes: &[Node], view: &LiveView, items: &mut Vec<Focus>) {
    for node in nodes {
        match node {
            Node::Form { id, action, fields } if !view.handled_actions.contains(action) => {
                for field in fields {
                    items.push(Focus::Field {
                        owner: view.owner.clone(),
                        view_id: view.view.id.clone(),
                        node_id: id.clone(),
                        action: action.clone(),
                        field: field.clone(),
                        revision: view.revision,
                    });
                }
            }
            Node::Button { action, .. } if !view.handled_actions.contains(action) => {
                items.push(Focus::Button {
                    owner: view.owner.clone(),
                    view_id: view.view.id.clone(),
                    action: action.clone(),
                    revision: view.revision,
                })
            }
            Node::Group { children, .. } => collect_focus(children, view, items),
            _ => {}
        }
    }
}
fn lines(nodes: &[Node], out: &mut Vec<Line<'static>>, depth: usize) {
    let prefix = "  ".repeat(depth);
    for node in nodes {
        match node {
            Node::Text { text, .. } => out.push(Line::raw(format!("{prefix}{text}"))),
            Node::Code { language, text, .. } => {
                out.push(Line::styled(
                    format!("{prefix}code {}", language.as_deref().unwrap_or("")),
                    Style::default().fg(Color::Blue),
                ));
                for line in text.lines() {
                    out.push(Line::raw(format!("{prefix}  {line}")));
                }
            }
            Node::Diff { before, after, .. } => {
                for line in before.lines() {
                    out.push(Line::styled(
                        format!("{prefix}- {line}"),
                        Style::default().fg(Color::Red),
                    ));
                }
                for line in after.lines() {
                    out.push(Line::styled(
                        format!("{prefix}+ {line}"),
                        Style::default().fg(Color::Green),
                    ));
                }
            }
            Node::Table { columns, rows, .. } => {
                out.push(Line::styled(
                    format!("{prefix}{}", columns.join(" │ ")),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                for row in rows {
                    out.push(Line::raw(format!("{prefix}{}", row.join(" │ "))));
                }
            }
            Node::Group {
                title, children, ..
            } => {
                out.push(Line::styled(
                    format!("{prefix}{title}"),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                lines(children, out, depth + 1);
            }
            Node::Attachment {
                name,
                record_sequence,
                ..
            } => out.push(Line::raw(format!(
                "{prefix}attachment: {name} (record {record_sequence})"
            ))),
            Node::Status { text, .. } => out.push(Line::styled(
                format!("{prefix}{text}"),
                Style::default().fg(Color::Yellow),
            )),
            Node::Form { fields, .. } => {
                out.push(Line::styled(
                    format!("{prefix}form (Tab to edit, Enter to submit)"),
                    Style::default().fg(Color::Cyan),
                ));
                for field in fields {
                    out.push(Line::raw(format!(
                        "{prefix}  {} [{}]",
                        field.label, field.id
                    )));
                }
            }
            Node::Button { label, .. } => out.push(Line::styled(
                format!("{prefix}[{label}] (Tab, Enter)"),
                Style::default().fg(Color::Cyan),
            )),
        }
    }
}
struct RawTerminal(Terminal<CrosstermBackend<Stdout>>);
impl RawTerminal {
    fn open() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        Ok(Self(Terminal::new(CrosstermBackend::new(stdout))?))
    }
}
impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.0.backend_mut(), LeaveAlternateScreen);
    }
}
fn draw(terminal: &mut RawTerminal, screen: &Screen) -> io::Result<()> {
    let focus = screen.current();
    terminal.0.draw(|frame| {
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(5),
                Constraint::Length(5),
            ])
            .split(frame.area());
        let peers = screen
            .snapshot
            .activity
            .iter()
            .filter(|activity| activity.frontend != "tui")
            .map(|activity| format!("{} editing {:?}", activity.frontend, activity.target))
            .collect::<Vec<_>>()
            .join(" · ");
        frame.render_widget(
            Paragraph::new(format!(
                "Session {} · run {:?} · {}",
                screen.snapshot.session_id, screen.active_run, peers
            )),
            areas[0],
        );
        let mut content = Vec::new();
        for view in &screen.snapshot.views {
            content.push(Line::from(vec![Span::styled(
                format!("{} / {}  {}", view.owner, view.view.id, view.view.title),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )]));
            if !view.view.platforms.is_empty()
                && !view.view.platforms.iter().any(|platform| platform == "tui")
            {
                content.push(Line::raw(format!(
                    "  {} — unavailable in terminal",
                    view.view.fallback
                )));
            } else {
                lines(&view.view.nodes, &mut content, 1);
            }
            if !view.handled_actions.is_empty() {
                content.push(Line::raw(format!(
                    "  handled: {}",
                    view.handled_actions.join(", ")
                )));
            }
            if !view.active {
                content.push(Line::raw("  settled; actions unavailable"));
            }
        }
        if content.is_empty() {
            content.push(Line::raw("Waiting for a live view"));
        }
        frame.render_widget(
            Paragraph::new(content)
                .block(Block::default().title("Presentation").borders(Borders::ALL)),
            areas[1],
        );
        let focused = match focus {
            Focus::Composer => format!("Composer: {}", screen.composer),
            Focus::Field {
                owner,
                view_id,
                node_id,
                field,
                ..
            } => {
                let key = (owner, view_id, node_id, field.id.clone());
                format!(
                    "{}: {}",
                    field.label,
                    screen
                        .drafts
                        .get(&key)
                        .map_or(String::new(), Value::to_string)
                )
            }
            Focus::Button { action, .. } => format!("Action: {action}"),
        };
        frame.render_widget(
            Paragraph::new(format!(
                "{focused}\n{}\nTab focus · Enter submit · x cancel run · q detach",
                screen.message
            ))
            .block(Block::default().title("Input").borders(Borders::ALL)),
            areas[2],
        );
    })?;
    Ok(())
}
fn draft_key(owner: &str, view_id: &str, node_id: &str, field_id: &str) -> DraftKey {
    (
        owner.into(),
        view_id.into(),
        node_id.into(),
        field_id.into(),
    )
}
async fn report(endpoint: &Path, attachment: u64, target: ActivityTarget, active: bool) {
    let _ = call(
        endpoint,
        "POST",
        "/activity",
        Some(&json!({ "attachment": attachment, "target": target, "active": active })),
    )
    .await;
}
fn find_form<'a>(nodes: &'a [Node], id: &str) -> Option<&'a [Field]> {
    for node in nodes {
        match node {
            Node::Form {
                id: found, fields, ..
            } if found == id => return Some(fields),
            Node::Group { children, .. } => {
                if let Some(fields) = find_form(children, id) {
                    return Some(fields);
                }
            }
            _ => {}
        }
    }
    None
}
fn form_values(screen: &Screen, owner: &str, view_id: &str, node_id: &str) -> Value {
    let mut values = serde_json::Map::new();
    if let Some(view) = screen
        .snapshot
        .views
        .iter()
        .find(|view| view.owner == owner && view.view.id == view_id)
        && let Some(fields) = find_form(&view.view.nodes, node_id)
    {
        for field in fields {
            if let Some(initial) = &field.initial {
                values.insert(field.id.clone(), initial.clone());
            } else if field.kind == FieldKind::Boolean {
                values.insert(field.id.clone(), json!(false));
            }
        }
    }
    for ((o, v, n, field), value) in &screen.drafts {
        if o == owner && v == view_id && n == node_id {
            values.insert(field.clone(), value.clone());
        }
    }
    Value::Object(values)
}
/// Use a real terminal to control one explicitly selected live host.
pub async fn run(endpoint: &Path) -> Result<i32, Box<dyn std::error::Error>> {
    let attached = call(
        endpoint,
        "POST",
        "/attach",
        Some(&json!({ "frontend": "tui" })),
    )
    .await?;
    let attachment = attached["attachment"]
        .as_u64()
        .ok_or("missing attachment")?;
    let result = run_attached(endpoint, attachment).await;
    let _ = call(
        endpoint,
        "POST",
        "/detach",
        Some(&json!({ "attachment": attachment })),
    )
    .await;
    result
}
async fn run_attached(endpoint: &Path, attachment: u64) -> Result<i32, Box<dyn std::error::Error>> {
    let initial = call(
        endpoint,
        "GET",
        &format!("/snapshot?attachment={attachment}"),
        None,
    )
    .await?;
    let snapshot: Snapshot = serde_json::from_value(initial["presentation"].clone())?;
    let mut screen = Screen {
        snapshot,
        active_run: initial["state"]["active_run"].as_u64(),
        composer: String::new(),
        drafts: BTreeMap::new(),
        focus: 0,
        message: String::new(),
        request_number: 0,
        last_activity: None,
    };
    let mut terminal = RawTerminal::open()?;
    let endpoint_owned = endpoint.to_owned();
    let (updates, mut receiver) = tokio::sync::watch::channel(initial);
    let follower = tokio::spawn(async move {
        let mut sequence = updates.borrow()["presentation"]["sequence"]
            .as_u64()
            .unwrap_or(0);
        while let Ok(value) = call(
            &endpoint_owned,
            "GET",
            &format!("/snapshot?after={sequence}&attachment={attachment}"),
            None,
        )
        .await
        {
            sequence = value["presentation"]["sequence"]
                .as_u64()
                .unwrap_or(sequence);
            updates.send_replace(value);
        }
    });
    loop {
        if receiver.has_changed().unwrap_or(false) {
            let value = receiver.borrow_and_update().clone();
            screen.snapshot = serde_json::from_value(value["presentation"].clone())?;
            screen.active_run = value["state"]["active_run"].as_u64();
        }
        draw(&mut terminal, &screen)?;
        if let Some((target, at)) = &screen.last_activity
            && at.elapsed() >= Duration::from_secs(3)
        {
            report(endpoint, attachment, target.clone(), false).await;
            screen.last_activity = None;
        }
        let event = tokio::task::spawn_blocking(|| -> io::Result<Option<Event>> {
            if event::poll(Duration::from_millis(120))? {
                Ok(Some(event::read()?))
            } else {
                Ok(None)
            }
        })
        .await??;
        let Some(Event::Key(key)) = event else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.code == KeyCode::Char('q') && key.modifiers == KeyModifiers::CONTROL {
            break;
        }
        if key.code == KeyCode::Esc {
            break;
        }
        if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
            let count = screen.focusables().len();
            screen.focus = if key.code == KeyCode::BackTab {
                (screen.focus + count - 1) % count
            } else {
                (screen.focus + 1) % count
            };
            continue;
        }
        if key.code == KeyCode::Char('x') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(run_id) = screen.active_run {
                screen.message = format!(
                    "{:?}",
                    call(
                        endpoint,
                        "POST",
                        "/cancel",
                        Some(&json!({ "run_id": run_id }))
                    )
                    .await
                );
            }
            continue;
        }
        match screen.current() {
            Focus::Composer => match key.code {
                KeyCode::Char(character) => {
                    screen.composer.push(character);
                    let target = ActivityTarget::Composer;
                    report(endpoint, attachment, target.clone(), true).await;
                    screen.last_activity = Some((target, Instant::now()));
                }
                KeyCode::Backspace => {
                    screen.composer.pop();
                }
                KeyCode::Enter if !screen.composer.trim().is_empty() => {
                    let body =
                        json!({ "request_id": screen.request_id(), "text": screen.composer });
                    match call(endpoint, "POST", "/prompt", Some(&body)).await {
                        Ok(value) => {
                            screen.message = format!("accepted run {}", value["run_id"]);
                            screen.composer.clear();
                        }
                        Err(error) => screen.message = error.to_string(),
                    }
                    report(endpoint, attachment, ActivityTarget::Composer, false).await;
                    screen.last_activity = None;
                }
                _ => {}
            },
            Focus::Field {
                owner,
                view_id,
                node_id,
                action,
                field,
                revision,
            } => {
                let target = ActivityTarget::Form {
                    owner: owner.clone(),
                    view_id: view_id.clone(),
                    node_id: node_id.clone(),
                };
                let key_name = draft_key(&owner, &view_id, &node_id, &field.id);
                match key.code {
                    KeyCode::Enter => {
                        let request = ActionRequest {
                            session_id: screen.snapshot.session_id,
                            owner: owner.clone(),
                            view_id: view_id.clone(),
                            revision,
                            action,
                            request_id: screen.request_id(),
                            values: form_values(&screen, &owner, &view_id, &node_id),
                        };
                        screen.message =
                            match call(endpoint, "POST", "/action", Some(&json!(request))).await {
                                Ok(value) => format!("action: {value}"),
                                Err(error) => error.to_string(),
                            };
                        report(endpoint, attachment, target, false).await;
                        screen.last_activity = None;
                    }
                    KeyCode::Char(character) if field.kind == FieldKind::Text => {
                        let draft = screen.drafts.entry(key_name).or_insert(json!(""));
                        let mut value = draft.as_str().unwrap_or("").to_owned();
                        value.push(character);
                        *draft = json!(value);
                        report(endpoint, attachment, target.clone(), true).await;
                        screen.last_activity = Some((target, Instant::now()));
                    }
                    KeyCode::Backspace if field.kind == FieldKind::Text => {
                        let draft = screen.drafts.entry(key_name).or_insert(json!(""));
                        let mut value = draft.as_str().unwrap_or("").to_owned();
                        value.pop();
                        *draft = json!(value);
                        report(endpoint, attachment, target.clone(), true).await;
                        screen.last_activity = Some((target, Instant::now()));
                    }
                    KeyCode::Char(' ') if field.kind == FieldKind::Boolean => {
                        let previous = screen
                            .drafts
                            .get(&key_name)
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        screen.drafts.insert(key_name, json!(!previous));
                        report(endpoint, attachment, target.clone(), true).await;
                        screen.last_activity = Some((target, Instant::now()));
                    }
                    KeyCode::Left | KeyCode::Right
                        if field.kind == FieldKind::Choice && !field.options.is_empty() =>
                    {
                        let index = screen
                            .drafts
                            .get(&key_name)
                            .and_then(Value::as_str)
                            .and_then(|selected| {
                                field.options.iter().position(|option| option == selected)
                            })
                            .unwrap_or(0);
                        let next = if key.code == KeyCode::Left {
                            (index + field.options.len() - 1) % field.options.len()
                        } else {
                            (index + 1) % field.options.len()
                        };
                        screen.drafts.insert(key_name, json!(field.options[next]));
                        report(endpoint, attachment, target.clone(), true).await;
                        screen.last_activity = Some((target, Instant::now()));
                    }
                    KeyCode::Char(number @ '1'..='9') if field.kind == FieldKind::MultiChoice => {
                        let index = number as usize - '1' as usize;
                        if let Some(option) = field.options.get(index) {
                            let draft = screen.drafts.entry(key_name).or_insert(json!([]));
                            let mut selected: Vec<String> =
                                serde_json::from_value(draft.clone()).unwrap_or_default();
                            if let Some(position) = selected.iter().position(|item| item == option)
                            {
                                selected.remove(position);
                            } else {
                                selected.push(option.clone());
                            }
                            *draft = json!(selected);
                            report(endpoint, attachment, target.clone(), true).await;
                            screen.last_activity = Some((target, Instant::now()));
                        }
                    }
                    _ => {}
                }
            }
            Focus::Button {
                owner,
                view_id,
                action,
                revision,
            } if key.code == KeyCode::Enter => {
                let request = ActionRequest {
                    session_id: screen.snapshot.session_id,
                    owner,
                    view_id,
                    revision,
                    action,
                    request_id: screen.request_id(),
                    values: Value::Null,
                };
                screen.message =
                    match call(endpoint, "POST", "/action", Some(&json!(request))).await {
                        Ok(value) => format!("action: {value}"),
                        Err(error) => error.to_string(),
                    };
            }
            _ => {}
        }
    }
    follower.abort();
    Ok(0)
}
