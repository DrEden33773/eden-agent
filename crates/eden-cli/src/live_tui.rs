//! Minimal keyboard consumer of the shared semantic presentation vocabulary.
use crate::live::call;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use eden_protocol::Fault;
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
    collections::{BTreeMap, BTreeSet},
    io::{self, Stdout},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
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
impl Focus {
    fn identity(&self) -> (&str, &str, &str, &str) {
        match self {
            Self::Composer => ("", "", "", ""),
            Self::Field {
                owner,
                view_id,
                node_id,
                field,
                ..
            } => (owner, view_id, node_id, &field.id),
            Self::Button {
                owner,
                view_id,
                action,
                ..
            } => (owner, view_id, "", action),
        }
    }
}
struct Follower(tokio::task::JoinHandle<()>);
impl Drop for Follower {
    fn drop(&mut self) {
        self.0.abort();
    }
}
struct Screen {
    snapshot: Snapshot,
    active_run: Option<u64>,
    read_only: bool,
    composer: String,
    drafts: BTreeMap<DraftKey, Value>,
    option_cursor: BTreeMap<DraftKey, usize>,
    focus: usize,
    message: String,
    request_number: u64,
    attempts: BTreeMap<String, String>,
    last_uncertain: Option<(String, Value, String)>,
    last_activity: Option<(ActivityTarget, Instant)>,
    pending: BTreeSet<String>,
    scroll: (u16, u16),
    viewport: u16,
    content_size: (u16, u16),
    connection: String,
}
impl Screen {
    fn focusables(&self) -> Vec<Focus> {
        let mut items = vec![Focus::Composer];
        for view in &self.snapshot.views {
            if view.active
                && (view.view.platforms.is_empty()
                    || view.view.platforms.iter().any(|platform| platform == "tui"))
            {
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
                    let hint = match field.kind {
                        FieldKind::Choice => format!(" ←/→: {}", field.options.join(", ")),
                        FieldKind::MultiChoice => {
                            format!(" ←/→ choose, Space toggle: {}", field.options.join(", "))
                        }
                        FieldKind::Boolean => " Space toggles".into(),
                        FieldKind::Text => String::new(),
                    };
                    out.push(Line::raw(format!(
                        "{prefix}  {} [{}]{hint}",
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
fn draw(terminal: &mut RawTerminal, screen: &mut Screen) -> io::Result<()> {
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
            Paragraph::new(if screen.read_only {
                format!(
                    "Session {} · saved presentation · read-only · {}",
                    screen.snapshot.session_id, screen.connection
                )
            } else {
                format!(
                    "Session {} · run {:?} · {} · {}",
                    screen.snapshot.session_id, screen.active_run, screen.connection, peers
                )
            }),
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
        screen.viewport = areas[1].height.saturating_sub(2);
        screen.content_size = (
            content.len().min(u16::MAX as usize) as u16,
            content
                .iter()
                .map(Line::width)
                .max()
                .unwrap_or(0)
                .min(u16::MAX as usize) as u16,
        );
        screen.scroll.0 = screen
            .scroll
            .0
            .min(screen.content_size.0.saturating_sub(screen.viewport));
        screen.scroll.1 = screen.scroll.1.min(
            screen
                .content_size
                .1
                .saturating_sub(areas[1].width.saturating_sub(2)),
        );
        frame.render_widget(
            Paragraph::new(content).scroll(screen.scroll).block(
                Block::default()
                    .title("Presentation · PgUp/PgDn · Alt+←/→")
                    .borders(Borders::ALL),
            ),
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
                let value = field_value(screen, &key, &field).to_string();
                let option = if field.kind == FieldKind::MultiChoice && !field.options.is_empty() {
                    let index =
                        screen.option_cursor.get(&key).copied().unwrap_or(0) % field.options.len();
                    format!(
                        " · option {}/{}: {} (←/→, Space)",
                        index + 1,
                        field.options.len(),
                        field.options[index]
                    )
                } else {
                    String::new()
                };
                format!("{}: {}{}", field.label, value, option)
            }
            Focus::Button { action, .. } => format!("Action: {action}"),
        };
        frame.render_widget(
            Paragraph::new(if screen.read_only {
                "PgUp/PgDn ↑/↓ scroll · Alt+←/→ pan\nHome/End · Esc to close".to_owned()
            } else {
                format!(
                    "{focused}\n{}\nTab · Enter · Ctrl+x cancel · Ctrl+r retry · Esc",
                    screen.message
                )
            })
            .block(Block::default().title("Input").borders(Borders::ALL)),
            areas[2],
        );
    })?;
    Ok(())
}
fn definite_reply(error: &Fault) -> bool {
    !(error.source == "live"
        && ["Unavailable", "InputFailure", "OutputFailure"].contains(&error.code.as_str()))
}
struct Reply {
    route: String,
    body: Value,
    key: String,
    result: Result<Value, Fault>,
}
fn submit(
    screen: &mut Screen,
    replies: &tokio::sync::mpsc::UnboundedSender<Reply>,
    endpoint: &Path,
    route: &str,
    mut body: Value,
) {
    if let Some(object) = body.as_object_mut() {
        object.remove("request_id");
    }
    let key = format!("{route}:{body}");
    if !screen.pending.insert(key.clone()) {
        return;
    }
    let id = if let Some(known) = screen.attempts.get(&key) {
        known.clone()
    } else {
        let id = screen.request_id();
        screen.attempts.insert(key.clone(), id.clone());
        id
    };
    body["request_id"] = json!(id);
    let endpoint = endpoint.to_owned();
    let route = route.to_owned();
    let replies = replies.clone();
    screen.message = "Request pending · Ctrl+x cancel · Esc detach".into();
    // Dropping the frontend only drops its response receiver; the host still owns the action.
    tokio::spawn(async move {
        let result = call(&endpoint, "POST", &route, Some(&body)).await;
        let _ = replies.send(Reply {
            route,
            body,
            key,
            result,
        });
    });
}
fn receive_reply(screen: &mut Screen, reply: Reply) {
    screen.pending.remove(&reply.key);
    if reply.result.is_ok() || reply.result.as_ref().err().is_some_and(definite_reply) {
        screen.attempts.remove(&reply.key);
        if screen
            .last_uncertain
            .as_ref()
            .is_some_and(|(_, _, key)| key == &reply.key)
        {
            screen.last_uncertain = None;
        }
    } else {
        screen.last_uncertain = Some((reply.route.clone(), reply.body.clone(), reply.key));
    }
    if reply.result.is_ok() && reply.route == "/prompt" && reply.body["text"] == screen.composer {
        screen.composer.clear();
    }
    screen.message = match reply.result {
        Ok(value) => format!("{}: {value}", reply.route),
        Err(error) => error.to_string(),
    };
}
fn field_value(screen: &Screen, key: &DraftKey, field: &Field) -> Value {
    screen
        .drafts
        .get(key)
        .cloned()
        .or_else(|| field.initial.clone())
        .unwrap_or_else(|| match field.kind {
            FieldKind::Text => json!(""),
            FieldKind::Boolean => json!(false),
            FieldKind::Choice => field
                .options
                .first()
                .map_or(Value::Null, |value| json!(value)),
            FieldKind::MultiChoice => json!([]),
        })
}
// Compatibility is view-local: unfamiliar vocabulary cannot disable known sibling views.
fn decode_snapshot(mut value: Value) -> Result<Snapshot, serde_json::Error> {
    let compatible = value["version"] == eden_protocol::presentation::VERSION;
    if let Some(views) = value["views"].as_array_mut() {
        for view in views {
            let known_slot =
                serde_json::from_value::<eden_protocol::presentation::Slot>(view["slot"].clone())
                    .is_ok();
            let mut nodes = if compatible && known_slot {
                view["nodes"]
                    .as_array()
                    .map(|nodes| nodes.iter().flat_map(compatible_nodes).collect::<Vec<_>>())
                    .unwrap_or_default()
            } else {
                view["active"] = json!(false);
                vec![]
            };
            if nodes.is_empty() {
                nodes.push(json!({
                    "kind": "text",
                    "id": "compatibility-fallback",
                    "text": view["fallback"]
                        .as_str()
                        .unwrap_or("Unsupported presentation"),
                }));
            }
            if !known_slot {
                view["slot"] = json!("panel");
            }
            view["nodes"] = Value::Array(nodes);
        }
    }
    serde_json::from_value(value)
}
fn compatible_nodes(raw: &Value) -> Vec<Value> {
    let mut node = raw.clone();
    let children = raw["children"]
        .as_array()
        .map(|items| items.iter().flat_map(compatible_nodes).collect::<Vec<_>>())
        .unwrap_or_default();
    if raw["kind"] == "group" {
        node["children"] = json!(children);
    }
    if serde_json::from_value::<Node>(node.clone()).is_ok() {
        vec![node]
    } else {
        let mut fallback = vec![json!({
            "kind": "text",
            "id": raw["id"].as_str().unwrap_or("unknown-node"),
            "text": raw["fallback"]
                .as_str()
                .unwrap_or("Unsupported presentation node"),
        })];
        fallback.extend(children);
        fallback
    }
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
    let _ = tokio::time::timeout(
        Duration::from_millis(200),
        call(
            endpoint,
            "POST",
            "/activity",
            Some(&json!({ "attachment": attachment, "target": target, "active": active })),
        ),
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
            let key = draft_key(owner, view_id, node_id, &field.id);
            values.insert(field.id.clone(), field_value(screen, &key, field));
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
    let lease = Arc::new(AtomicU64::new(attachment));
    let result = run_attached(endpoint, lease.clone()).await;
    let attachment = lease.load(Ordering::Relaxed);
    let _ = call(
        endpoint,
        "POST",
        "/detach",
        Some(&json!({ "attachment": attachment })),
    )
    .await;
    result
}
async fn run_attached(
    endpoint: &Path,
    lease: Arc<AtomicU64>,
) -> Result<i32, Box<dyn std::error::Error>> {
    let attachment = lease.load(Ordering::Relaxed);
    let initial = call(
        endpoint,
        "GET",
        &format!("/snapshot?attachment={attachment}"),
        None,
    )
    .await?;
    let snapshot: Snapshot = decode_snapshot(initial["presentation"].clone())?;
    let mut screen = Screen {
        snapshot,
        active_run: initial["state"]["active_run"].as_u64(),
        read_only: initial["state"]["read_only"] == true,
        composer: String::new(),
        drafts: BTreeMap::new(),
        option_cursor: BTreeMap::new(),
        focus: 0,
        message: String::new(),
        request_number: 0,
        attempts: BTreeMap::new(),
        last_uncertain: None,
        last_activity: None,
        pending: BTreeSet::new(),
        scroll: (0, 0),
        viewport: 1,
        content_size: (0, 0),
        connection: "Connected".into(),
    };
    let mut terminal = RawTerminal::open()?;
    let endpoint_owned = endpoint.to_owned();
    let (updates, mut receiver) = tokio::sync::watch::channel((Some(initial), None::<String>));
    let follower_lease = lease.clone();
    let _follower = Follower(tokio::spawn(async move {
        let mut sequence = 0;
        loop {
            let attachment = follower_lease.load(Ordering::Relaxed);
            match tokio::time::timeout(
                Duration::from_secs(6),
                call(
                    &endpoint_owned,
                    "GET",
                    &format!("/snapshot?after={sequence}&attachment={attachment}"),
                    None,
                ),
            )
            .await
            .unwrap_or_else(|_| Err(Fault::new("Unavailable", "live", "snapshot timed out")))
            {
                Ok(value) => {
                    sequence = value["presentation"]["sequence"]
                        .as_u64()
                        .unwrap_or(sequence);
                    updates.send_replace((Some(value), None));
                }
                Err(error) => {
                    updates.send_replace((None, Some(format!("Disconnected · retrying: {error}"))));
                    if error.source == "presentation"
                        && error.code == "InvalidInput"
                        && let Ok(attached) = call(
                            &endpoint_owned,
                            "POST",
                            "/attach",
                            Some(&json!({ "frontend": "tui" })),
                        )
                        .await
                        && let Some(attachment) = attached["attachment"].as_u64()
                    {
                        follower_lease.store(attachment, Ordering::Relaxed);
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        }
    }));
    let (replies, mut results) = tokio::sync::mpsc::unbounded_channel();
    loop {
        let attachment = lease.load(Ordering::Relaxed);
        if receiver.has_changed().unwrap_or(false) {
            let (value, error) = receiver.borrow_and_update().clone();
            screen.connection = error.unwrap_or_else(|| "Connected".into());
            if let Some(value) = value {
                let snapshot: Snapshot = decode_snapshot(value["presentation"].clone())?;
                // A new run owns a new set of drafts even when an author reuses node IDs.
                let active_run = value["state"]["active_run"].as_u64();
                if active_run.is_some() && active_run != screen.active_run {
                    screen.drafts.clear();
                    screen.option_cursor.clear();
                }
                let focused = screen.current();
                screen.snapshot = snapshot;
                screen.focus = screen
                    .focusables()
                    .iter()
                    .position(|item| item.identity() == focused.identity())
                    .unwrap_or(0);
                screen.active_run = active_run;
                screen.read_only = value["state"]["read_only"] == true;
            }
        }
        while let Ok(reply) = results.try_recv() {
            receive_reply(&mut screen, reply);
        }
        draw(&mut terminal, &mut screen)?;
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
        let scroll_key = match key.code {
            KeyCode::PageDown => {
                screen.scroll.0 = screen.scroll.0.saturating_add(screen.viewport.max(1));
                true
            }
            KeyCode::PageUp => {
                screen.scroll.0 = screen.scroll.0.saturating_sub(screen.viewport.max(1));
                true
            }
            KeyCode::Down => {
                screen.scroll.0 = screen.scroll.0.saturating_add(1);
                true
            }
            KeyCode::Up => {
                screen.scroll.0 = screen.scroll.0.saturating_sub(1);
                true
            }
            KeyCode::Home => {
                screen.scroll = (0, 0);
                true
            }
            KeyCode::End => {
                screen.scroll.0 = screen.content_size.0;
                true
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => {
                screen.scroll.1 = screen.scroll.1.saturating_add(8);
                true
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => {
                screen.scroll.1 = screen.scroll.1.saturating_sub(8);
                true
            }
            _ => false,
        };
        if scroll_key || screen.read_only {
            continue;
        }
        if key.code == KeyCode::Enter && screen.connection != "Connected" {
            screen.message = "Waiting for connection before submitting".into();
            continue;
        }
        if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
            if let Some((target, _)) = screen.last_activity.take() {
                report(endpoint, attachment, target, false).await;
            }
            let count = screen.focusables().len();
            screen.focus = if key.code == KeyCode::BackTab {
                (screen.focus + count - 1) % count
            } else {
                (screen.focus + 1) % count
            };
            continue;
        }
        if key.code == KeyCode::Char('x') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some((target, _)) = screen.last_activity.take() {
                report(endpoint, attachment, target, false).await;
            }
            if let Some(run_id) = screen.active_run {
                submit(
                    &mut screen,
                    &replies,
                    endpoint,
                    "/cancel",
                    json!({ "run_id": run_id }),
                );
            }
            continue;
        }
        if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some((route, body, _)) = screen.last_uncertain.clone() {
                submit(&mut screen, &replies, endpoint, &route, body);
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
                    if screen.composer.pop().is_some() {
                        let target = ActivityTarget::Composer;
                        report(endpoint, attachment, target.clone(), true).await;
                        screen.last_activity = Some((target, Instant::now()));
                    }
                }
                KeyCode::Enter if !screen.composer.trim().is_empty() => {
                    report(endpoint, attachment, ActivityTarget::Composer, false).await;
                    screen.last_activity = None;
                    let body = json!({ "text": screen.composer });
                    submit(&mut screen, &replies, endpoint, "/prompt", body);
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
                            request_id: String::new(),
                            values: form_values(&screen, &owner, &view_id, &node_id),
                        };
                        report(endpoint, attachment, target, false).await;
                        screen.last_activity = None;
                        submit(&mut screen, &replies, endpoint, "/action", json!(request));
                    }
                    KeyCode::Char(character) if field.kind == FieldKind::Text => {
                        let initial = field_value(&screen, &key_name, &field);
                        let draft = screen.drafts.entry(key_name).or_insert(initial);
                        let mut value = draft.as_str().unwrap_or("").to_owned();
                        value.push(character);
                        *draft = json!(value);
                        report(endpoint, attachment, target.clone(), true).await;
                        screen.last_activity = Some((target, Instant::now()));
                    }
                    KeyCode::Backspace if field.kind == FieldKind::Text => {
                        let initial = field_value(&screen, &key_name, &field);
                        let draft = screen.drafts.entry(key_name).or_insert(initial);
                        let mut value = draft.as_str().unwrap_or("").to_owned();
                        value.pop();
                        *draft = json!(value);
                        report(endpoint, attachment, target.clone(), true).await;
                        screen.last_activity = Some((target, Instant::now()));
                    }
                    KeyCode::Char(' ') if field.kind == FieldKind::Boolean => {
                        let previous = field_value(&screen, &key_name, &field)
                            .as_bool()
                            .unwrap_or(false);
                        screen.drafts.insert(key_name, json!(!previous));
                        report(endpoint, attachment, target.clone(), true).await;
                        screen.last_activity = Some((target, Instant::now()));
                    }
                    KeyCode::Left | KeyCode::Right
                        if field.kind == FieldKind::Choice && !field.options.is_empty() =>
                    {
                        let current = field_value(&screen, &key_name, &field);
                        let index = current
                            .as_str()
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
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(',') | KeyCode::Char('.')
                        if field.kind == FieldKind::MultiChoice && !field.options.is_empty() =>
                    {
                        let index = screen.option_cursor.get(&key_name).copied().unwrap_or(0);
                        let next = if matches!(key.code, KeyCode::Left | KeyCode::Char(',')) {
                            (index + field.options.len() - 1) % field.options.len()
                        } else {
                            (index + 1) % field.options.len()
                        };
                        screen.option_cursor.insert(key_name, next);
                    }
                    KeyCode::Char(' ') if field.kind == FieldKind::MultiChoice => {
                        let index = screen.option_cursor.get(&key_name).copied().unwrap_or(0);
                        if let Some(option) = field.options.get(index) {
                            let initial = field_value(&screen, &key_name, &field);
                            let draft = screen.drafts.entry(key_name).or_insert(initial);
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
                    KeyCode::Char(number @ '1'..='9') if field.kind == FieldKind::MultiChoice => {
                        let index = number as usize - '1' as usize;
                        if let Some(option) = field.options.get(index) {
                            let initial = field_value(&screen, &key_name, &field);
                            let draft = screen.drafts.entry(key_name).or_insert(initial);
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
                    request_id: String::new(),
                    values: Value::Null,
                };
                submit(&mut screen, &replies, endpoint, "/action", json!(request));
            }
            _ => {}
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(nodes: Value, version: u32, slot: &str) -> Value {
        json!({
            "version": version,
            "session_id": "123",
            "sequence": 1,
            "activity": [],
            "pending_interactions": [],
            "views": [{
                "owner": "author",
                "run_id": 1,
                "revision": 1,
                "active": true,
                "id": "view",
                "slot": slot,
                "title": "Title",
                "fallback": "View fallback",
                "source": null,
                "platforms": [],
                "nodes": nodes,
            }],
        })
    }

    #[test]
    fn unknown_node_preserves_fallback_and_known_children() {
        let raw = snapshot(
            json!([{
                "kind": "group",
                "id": "group",
                "title": "Group",
                "children": [{
                    "kind": "future",
                    "id": "unknown",
                    "fallback": "Node fallback",
                    "children": [{ "kind": "text", "id": "known", "text": "Known child" }],
                }],
            }]),
            1,
            "panel",
        );
        let snapshot = decode_snapshot(raw).unwrap();
        let mut rendered = vec![];
        lines(&snapshot.views[0].view.nodes, &mut rendered, 0);
        let rendered = rendered
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Node fallback"));
        assert!(rendered.contains("Known child"));
        assert!(snapshot.views[0].active);
    }

    #[test]
    fn future_version_or_slot_is_readable_without_actions() {
        for (version, slot) in [(999, "panel"), (1, "future")] {
            let raw = snapshot(
                json!([{ "kind": "button", "id": "button", "label": "Unsafe", "action": "submit" }]),
                version,
                slot,
            );
            let snapshot = decode_snapshot(raw).unwrap();
            assert!(!snapshot.views[0].active);
            assert_eq!(
                snapshot.views[0].view.nodes,
                vec![Node::Text {
                    id: "compatibility-fallback".into(),
                    text: "View fallback".into()
                }]
            );
        }
    }
}
