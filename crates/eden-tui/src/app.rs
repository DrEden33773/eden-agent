use crate::{model::*, plugin::Editor, storage::DraftStore, text};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use eden_protocol::Fault;
use eden_tui_client::Snapshot;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub type LayoutKey = (u64, usize, u64, bool, bool, bool, bool, bool, u64);
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub enum Update {
    Attachment {
        generation: u64,
        result: std::result::Result<Attachment, String>,
    },
    Snapshot(Box<Snapshot>, Vec<Message>),
    Artifact {
        record: u64,
        result: std::result::Result<String, String>,
    },
    Completion {
        replacement: crate::autocomplete::Replacement,
        insert: String,
        source: String,
        cursor: usize,
        generation: u64,
        result: std::result::Result<Attachment, String>,
    },
    Refreshed {
        index: usize,
        before: Attachment,
        result: std::result::Result<Attachment, String>,
    },
    Clipboard(
        ClipboardRequest,
        std::result::Result<crate::clipboard::ClipboardContent, String>,
    ),
    Copied(std::result::Result<crate::clipboard::CopyOutcome, String>),
    Connection(String),
    Reply {
        route: String,
        body: Value,
        result: std::result::Result<Value, Fault>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardRequest {
    generation: u64,
    target: PasteTarget,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum PasteTarget {
    Composer(u64),
    Field {
        owner: String,
        view: String,
        node: String,
        path: String,
        binding: Option<eden_protocol::configuration_form::Binding>,
        kind: String,
        private: bool,
    },
}
pub struct App {
    pub references: Vec<std::sync::Arc<eden_protocol::session_reference::Reference>>,
    pub reference_picker: crate::references::Picker,
    pub context: crate::context::Context,
    pub editor: Editor,
    pub field_editor: Editor,
    pub artifact: Option<(u64, Message)>,
    undo: Vec<Draft>,
    redo: Vec<Draft>,
    checkpoint: Draft,
    input_history: Vec<String>,
    history_position: Option<usize>,
    history_draft: Draft,
    pub messages: Vec<Message>,
    pub transcript_images: crate::transcript_images::Thumbnails,
    pub selected: usize,
    pub preferences: Preferences,
    pub focus: Focus,
    pub dialog: Option<Dialog>,
    pub dialog_scroll: usize,
    pub notice: String,
    pub phase: Phase,
    pub anchor: Anchor,
    pub follow: bool,
    pub unseen: usize,
    pub rows: Vec<Row>,
    pub top: usize,
    pub viewport: usize,
    pub document_height: usize,
    pub inspector_scroll: usize,
    pub inspector_cache: Option<crate::view::InspectorLayout>,
    pub selection: Option<crate::selection::Selection>,
    pub autocomplete: crate::autocomplete::Autocomplete,
    pub attachments: Vec<Attachment>,
    pub recovery: Option<SavedDraft>,
    pub session: String,
    pub frontend: String,
    pub model: String,
    pub connection: String,
    pub quit: bool,
    pub queue: Vec<(String, String)>,
    pub store: Option<DraftStore>,
    pub saved: bool,
    pub edited_at: Instant,
    pub run_started: Instant,
    pub animation_frame: u64,
    pub row_keys: Vec<LayoutKey>,
    pub row_starts: Vec<usize>,
    pub read_only: bool,
    pub hover: Option<(u16, u16)>,
    pub cache: HashMap<LayoutKey, Vec<Row>>,
    pub external_editor: bool,
    pub suspend: bool,
    pub copied: Option<String>,
    pub clipboard_read: bool,
    pub clipboard_generation: u64,
    pub clipboard_escape: Option<String>,
    pub snapshot: Snapshot,
    pub lease: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    activity: Option<(eden_protocol::presentation::ActivityTarget, Instant)>,
    activity_sent: Instant,
    attachment_generation: u64,
    attachment_pending: usize,
    endpoint: PathBuf,
    tx: mpsc::Sender<Update>,
    rx: mpsc::Receiver<Update>,
    runtime: tokio::runtime::Handle,
    pending: Option<(String, Value, Draft)>,
    uncertain: Option<(String, Value, Draft)>,
    sessions: BTreeMap<String, Draft>,
    resources: eden_protocol::resources::Snapshot,
    commands: Vec<eden_protocol::resources::CommandDefinition>,
    form_target: Option<crate::forms::Target>,
    form_retry: Option<(String, Value, crate::forms::Target)>,
    form_applied: bool,
    form_refresh: Option<u64>,
    uncertain_checked: bool,
    form_drafts: BTreeMap<String, (crate::forms::Target, Dialog)>,
    pub theme: Option<crate::extensions::Theme>,
    pub renderer: Option<crate::extensions::Renderer>,
}
pub fn project_messages(snapshot: &Snapshot) -> Vec<Message> {
    let mut messages = crate::projection::history(&snapshot.history);
    messages.extend(crate::projection::streaming_with_history(
        &snapshot.events,
        snapshot.state.active_run,
        &snapshot.history,
    ));
    messages.extend(crate::projection::shell_streaming(
        &snapshot.events,
        &snapshot.state.shell_runs,
    ));
    messages
}
impl App {
    pub fn new(
        plugin: &Path,
        state: Option<&Path>,
        frontend: &str,
        endpoint: &Path,
        snapshot: Snapshot,
        tx: mpsc::Sender<Update>,
        rx: mpsc::Receiver<Update>,
    ) -> Result<Self> {
        let store = state.map(|p| DraftStore::new(p, frontend)).transpose()?;
        let loaded = store.as_ref().and_then(DraftStore::load);
        let session = snapshot.presentation.session_id.to_string();
        let sessions = loaded
            .as_ref()
            .map(|s| s.sessions.clone())
            .unwrap_or_default();
        let uncertain = loaded
            .as_ref()
            .filter(|s| s.session == session)
            .and_then(|s| s.pending.clone())
            .map(|p| (p.route, p.body, p.draft));
        let recovery = loaded.filter(|s| s.session == session || s.sessions.contains_key(&session));
        let cwd = if snapshot.state.cwd.is_empty() {
            std::env::current_dir()?
        } else {
            PathBuf::from(&snapshot.state.cwd)
        };
        let mut app = Self {
            references: vec![],
            reference_picker: Default::default(),
            context: Default::default(),
            editor: Editor::load(plugin)?,
            field_editor: Editor::load(plugin)?,
            artifact: None,
            undo: vec![],
            redo: vec![],
            checkpoint: Draft::default(),
            input_history: vec![],
            history_position: None,
            history_draft: Draft::default(),
            messages: vec![],
            transcript_images: Default::default(),
            selected: 0,
            preferences: Preferences::default(),
            focus: Focus::Editor,
            dialog: None,
            dialog_scroll: 0,
            notice: String::new(),
            phase: Phase::Idle,
            anchor: Anchor::default(),
            follow: true,
            unseen: 0,
            rows: vec![],
            top: 0,
            viewport: 1,
            document_height: 0,
            inspector_scroll: 0,
            inspector_cache: None,
            selection: None,
            autocomplete: crate::autocomplete::Autocomplete::new(cwd),
            attachments: vec![],
            recovery,
            session,
            frontend: frontend.into(),
            model: "Model unavailable".into(),
            connection: "Connected".into(),
            quit: false,
            queue: vec![],
            store,
            saved: true,
            edited_at: Instant::now(),
            run_started: Instant::now(),
            animation_frame: 0,
            row_keys: vec![],
            row_starts: vec![],
            read_only: snapshot.state.read_only,
            hover: None,
            cache: HashMap::new(),
            external_editor: false,
            suspend: false,
            copied: None,
            clipboard_read: false,
            clipboard_generation: 0,
            clipboard_escape: None,
            snapshot,
            lease: None,
            activity: None,
            activity_sent: Instant::now() - std::time::Duration::from_secs(1),
            attachment_generation: 0,
            attachment_pending: 0,
            endpoint: endpoint.into(),
            tx,
            rx,
            runtime: tokio::runtime::Handle::current(),
            pending: None,
            uncertain,
            sessions,
            resources: Default::default(),
            commands: vec![],
            form_target: None,
            form_retry: None,
            form_applied: false,
            form_refresh: None,
            uncertain_checked: false,
            form_drafts: BTreeMap::new(),
            theme: std::env::var_os("EDEN_TUI_THEME")
                .map(|p| crate::extensions::Theme::load(Path::new(&p)))
                .transpose()?,
            renderer: std::env::var_os("EDEN_TUI_RENDERER")
                .map(|p| crate::extensions::Renderer::load(Path::new(&p)))
                .transpose()?,
        };
        app.project();
        app.dispatch("/resources", Value::Null);
        app.dispatch("/commands", Value::Null);
        Ok(app)
    }
    fn project(&mut self) {
        let messages = project_messages(&self.snapshot);
        self.apply_messages(messages);
    }
    fn apply_messages(&mut self, mut messages: Vec<Message>) {
        self.context.update_usage(&self.snapshot.history);
        self.rebind_applied_form();

        let expanded: BTreeMap<_, _> = self.messages.iter().map(|m| (m.id, m.expanded)).collect();
        for m in &mut messages {
            if let Some(old) = expanded.get(&m.id) {
                m.expanded = *old;
            }
        }
        if !self.follow
            && messages.last().map(|m| (m.id, m.revision, m.body.len()))
                != self
                    .messages
                    .last()
                    .map(|m| (m.id, m.revision, m.body.len()))
        {
            self.unseen += 1;
        }
        for (index, view) in self.snapshot.presentation.views.iter().enumerate() {
            let mut content = Vec::new();
            crate::forms::content(&view.view.nodes, &mut content);
            if !view.view.platforms.is_empty() && !view.view.platforms.iter().any(|s| s == "tui") {
                content = vec![view.view.fallback.clone()];
            }
            let mut message = Message::new(
                u64::MAX / 2 + index as u64,
                Role::Notice,
                format!("{} / {}", view.owner, view.view.title),
                content.join("\n"),
            );
            message.revision = view.revision;
            messages.push(message);
        }
        self.input_history = messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| m.body.clone())
            .collect();
        self.transcript_images.refresh(&messages);
        self.messages = messages;
        self.selected = self.selected.min(self.messages.len().saturating_sub(1));
        if let Some(record) = self
            .snapshot
            .history
            .iter()
            .rev()
            .find(|r| r.kind == "model_selection")
        {
            let target = &record.payload["selection"];
            self.model = format!(
                "{}/{} · {}",
                target["provider"].as_str().unwrap_or("?"),
                target["model"].as_str().unwrap_or("?"),
                target["thinking"].as_str().unwrap_or("default effort")
            );
        }
        self.read_only = self.snapshot.state.read_only;
        self.phase = if self.snapshot.state.active_run.is_some() {
            if self.phase == Phase::Cancelling {
                Phase::Cancelling
            } else {
                Phase::Running
            }
        } else {
            Phase::Idle
        };
    }
    fn clipboard_request(&self) -> Option<ClipboardRequest> {
        let target = match &self.dialog {
            None if self.focus == Focus::Editor => {
                PasteTarget::Composer(self.snapshot.presentation.session_id)
            }
            Some(Dialog::Form {
                fields, selected, ..
            }) => {
                let field = fields.get(*selected)?;
                if field.readonly {
                    return None;
                }
                let target = self.form_target.as_ref()?;
                PasteTarget::Field {
                    owner: target.owner.clone(),
                    view: target.view.clone(),
                    node: target.node.clone(),
                    path: field.key.clone(),
                    binding: target.binding.clone(),
                    kind: field.kind.clone(),
                    private: field.private,
                }
            }
            _ => return None,
        };
        Some(ClipboardRequest {
            generation: self.clipboard_generation,
            target,
        })
    }
    pub fn clipboard_worker(&mut self) {
        if self.clipboard_read {
            self.clipboard_read = false;
            if let Some(request) = self.clipboard_request() {
                let tx = self.tx.clone();
                self.runtime.spawn_blocking(move || {
                    let _ = tx.send(Update::Clipboard(request, crate::clipboard::read()));
                });
            } else {
                self.notice = "Focus the composer or an editable field before pasting".into();
            }
        }
        if let Some(text) = self.copied.take() {
            let tx = self.tx.clone();
            self.runtime.spawn_blocking(move || {
                let _ = tx.send(Update::Copied(crate::clipboard::write(&text)));
            });
        }
    }
    pub fn draft(&self) -> Draft {
        Draft {
            references: self.references.clone(),
            text: self.editor.text(),
            cursor: self.editor.cursor(),
            attachments: self.attachments.clone(),
        }
    }
    pub fn changed(&mut self) {
        let next = self.draft();
        if self.checkpoint.text != next.text
            || self.checkpoint.attachments != next.attachments
            || self.checkpoint.references != next.references
        {
            self.undo
                .push(std::mem::replace(&mut self.checkpoint, next));
            if self.undo.len() > 128 {
                self.undo.remove(0);
            }
            self.redo.clear();
        } else {
            self.checkpoint.cursor = next.cursor;
        }

        if self.dialog.is_none() {
            self.activity = Some((
                eden_protocol::presentation::ActivityTarget::Composer,
                Instant::now(),
            ));
        }
        self.saved = false;
        self.edited_at = Instant::now();
    }
    pub fn persist(&mut self) {
        if self.saved {
            return;
        }
        let draft = self.draft();
        self.sessions.insert(self.session.clone(), draft.clone());
        if let Some(store) = &self.store {
            store.save(SavedDraft {
                version: 1,
                session: self.session.clone(),
                frontend: self.frontend.clone(),
                draft,
                sessions: self.sessions.clone(),
                pending: self.pending.as_ref().or(self.uncertain.as_ref()).map(
                    |(route, body, draft)| PendingSubmission {
                        route: route.clone(),
                        body: body.clone(),
                        draft: draft.clone(),
                    },
                ),
            });
        }
        self.saved = true;
    }
    pub fn restore_recovery(&mut self) {
        if let Some(saved) = self.recovery.take() {
            let draft = saved.sessions.get(&self.session).unwrap_or(&saved.draft);
            let _ = self.editor.restore(&draft.text, draft.cursor);
            self.attachments = draft.attachments.clone();
            self.references = draft.references.clone();
            self.changed();
        }
    }
    pub(crate) fn id(&self) -> String {
        format!(
            "tui-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        )
    }
    fn request(&mut self, route: &str, mut body: Value, consume: bool) {
        if self.pending.is_some() {
            self.notice = "A submission is pending; draft retained".into();
            return;
        }
        if self.uncertain.is_some() {
            self.notice = "Admission unknown · Ctrl+R checks the original request".into();
            return;
        }
        if matches!(route, "/prompt" | "/enqueue") {
            body["references"] = json!(self.references);
        }
        body["request_id"] = json!(self.id());
        body["session_id"] = json!(self.snapshot.presentation.session_id);
        let draft = if consume {
            self.draft()
        } else {
            Draft::default()
        };
        self.pending = Some((route.into(), body.clone(), draft));
        self.saved = false;
        self.persist();
        self.phase = Phase::Waiting;
        self.dispatch(route, body);
    }
    pub(crate) fn dispatch(&self, route: &str, mut body: Value) {
        if body.is_null() {
            body = json!({});
        }
        body["session_id"] = json!(self.snapshot.presentation.session_id);
        let endpoint = self.endpoint.clone();
        let route = route.to_owned();
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let result = eden_tui_client::call(&endpoint, "POST", &route, Some(&body)).await;
            let _ = tx.send(Update::Reply {
                route,
                body,
                result,
            });
        });
    }
    pub fn tick(&mut self) -> bool {
        if let Some((target, at)) = self.activity.clone()
            && let Some(lease) = &self.lease
        {
            let active = self.dialog.is_none()
                && !self.context.open
                && !self.reference_picker.open
                && self.focus == Focus::Editor
                && at.elapsed() < std::time::Duration::from_secs(3);
            if !active || self.activity_sent.elapsed() >= std::time::Duration::from_millis(250) {
                self.dispatch(
                    "/activity",
                    json!({
                        "attachment": lease.load(std::sync::atomic::Ordering::Relaxed),
                        "target": target,
                        "active": active,
                    }),
                );
                self.activity_sent = Instant::now();
                if !active {
                    self.activity = None;
                }
            }
        }

        if let Some(error) = self.store.as_ref().and_then(DraftStore::error) {
            self.notice = error;
        }
        let mut changed = self.autocomplete.poll()
            | self.poll_context_images()
            | self.transcript_images.poll()
            | self.poll_references();
        for _ in 0..64 {
            let Ok(update) = self.rx.try_recv() else {
                break;
            };
            changed = true;
            match update {
                Update::Refreshed {
                    index,
                    before,
                    result,
                } => {
                    if self.attachments.get(index) == Some(&before) {
                        match result {
                            Ok(attachment) => {
                                self.attachments[index] = attachment;
                                self.changed();
                            }
                            Err(error) => {
                                self.notice =
                                    format!("Refresh failed · original snapshot retained: {error}")
                            }
                        }
                    }
                }

                Update::Artifact { record, result } => match result {
                    Ok(text) => {
                        if self
                            .messages
                            .get(self.selected)
                            .is_some_and(|m| m.id == record)
                        {
                            let mut message =
                                Message::new(record, Role::Tool, "Complete retained output", text);
                            message.revision = u64::MAX;
                            self.artifact = Some((record, message));
                            self.inspector_cache = None;
                            self.inspector_scroll = 0;
                            self.preferences.inspector = true;
                        }
                    }
                    Err(error) => self.notice = format!("Full output unavailable: {error}"),
                },
                Update::Completion {
                    replacement,
                    insert,
                    source,
                    cursor,
                    generation,
                    result,
                } => {
                    self.attachment_pending = self.attachment_pending.saturating_sub(1);
                    match result {
                        Ok(attachment) => {
                            if generation == self.attachment_generation
                                && self.editor.text() == source
                                && self.editor.cursor() == cursor
                                && replacement.apply(&self.editor.text(), &insert).is_some()
                            {
                                let _ = self.editor.replace_range(replacement.token.range, &insert);
                                self.attachments.push(attachment);
                                self.changed();
                                self.sync_completion();
                            } else {
                                self.notice = "File completion changed while capturing · select \
                                               it again"
                                    .into();
                            }
                        }
                        Err(error) => self.notice = error,
                    }
                }
                Update::Attachment { generation, result } => {
                    if generation == self.attachment_generation {
                        self.attachment_pending = self.attachment_pending.saturating_sub(1);
                        match result {
                            Ok(attachment) => {
                                self.attachments.push(attachment);
                                self.changed();
                            }
                            Err(error) => self.notice = error,
                        }
                    }
                }
                Update::Snapshot(snapshot, messages) => {
                    if snapshot.presentation.session_id.to_string() != self.session {
                        self.connection =
                            "Endpoint changed session · detach and select explicitly".into();
                        continue;
                    }
                    self.snapshot = *snapshot;
                    self.connection = "Connected".into();
                    self.apply_messages(messages);
                }
                Update::Clipboard(request, result) => {
                    if self.clipboard_request().as_ref() != Some(&request) {
                        self.notice =
                            "Paste target changed · clipboard content was not inserted".into();
                        continue;
                    }
                    match result {
                        Ok(crate::clipboard::ClipboardContent::Text(text)) => self.paste(&text),
                        Ok(crate::clipboard::ClipboardContent::Image { media_type, bytes }) => {
                            if self.dialog.is_none() {
                                self.attachments.push(Attachment {
                                    name: format!(
                                        "clipboard.{}",
                                        media_type.split('/').nth(1).unwrap_or("png")
                                    ),
                                    source: String::new(),
                                    bytes: bytes.into(),
                                    media_type: Some(media_type),
                                    image: true,
                                });
                                self.changed();
                            } else {
                                self.notice = "Image paste belongs in the composer".into();
                            }
                        }
                        Err(error) => self.notice = error,
                    }
                }
                Update::Copied(result) => match result {
                    Ok(crate::clipboard::CopyOutcome::System) => {
                        self.notice = "Copied to system clipboard".into()
                    }
                    Ok(crate::clipboard::CopyOutcome::Osc52(sequence)) => {
                        self.clipboard_escape = Some(sequence)
                    }
                    Err(error) => self.notice = error,
                },
                Update::Connection(error) => {
                    self.clipboard_generation += 1;
                    self.connection = error;
                    if let Some(Dialog::Form { fields, .. }) = &mut self.dialog {
                        for field in fields.iter_mut().filter(|f| f.private) {
                            field.value.clear();
                            field.initial.clear();
                        }
                    }
                }
                Update::Reply {
                    route,
                    body,
                    result,
                } => {
                    if route.starts_with("/session/")
                        || route == "/reference/preview"
                        || (route == "/context/inspect" && body["reference_budget"].is_boolean())
                    {
                        self.reference_reply(&route, &body, result);
                        continue;
                    }
                    if route.starts_with("/context/")
                        || (route == "/terminal" && body["context_operation"].is_string())
                    {
                        self.context_reply(&route, &body, result);
                        continue;
                    }
                    if route == "/activity" {
                        continue;
                    }
                    if route == "/request-status" {
                        match result {
                            Ok(value) if value["status"] == "done" => {
                                if let Some((route, body, draft)) = self.uncertain.take() {
                                    self.pending = Some((route.clone(), body.clone(), draft));
                                    let result = serde_json::from_value(value["result"].clone())
                                        .unwrap_or_else(|_| {
                                            Err(Fault::new(
                                                "InvalidInput",
                                                "tui",
                                                "invalid receipt",
                                            ))
                                        });
                                    let _ = self.tx.send(Update::Reply {
                                        route,
                                        body,
                                        result,
                                    });
                                }
                            }
                            Ok(value) => {
                                self.uncertain_checked = value["status"] == "unknown";
                                if self.uncertain_checked {
                                    self.open("resolve-request");
                                }
                                self.notice = format!(
                                    "Original request: {} · draft retained",
                                    value["status"]
                                )
                            }
                            Err(error) => self.notice = error.to_string(),
                        }
                        continue;
                    }
                    let belongs =
                        self.pending
                            .as_ref()
                            .is_some_and(|(pending_route, pending_body, _)| {
                                pending_route == &route
                                    && pending_body["request_id"] == body["request_id"]
                            });
                    let pending = if belongs { self.pending.take() } else { None };
                    if belongs {
                        self.saved = false;
                    }

                    match result {
                        Ok(value) => {
                            if route == "/shutdown" {
                                self.quit = true;
                            }
                            if route == "/resources" {
                                if let Ok(resources) = serde_json::from_value(value.clone()) {
                                    self.resources = resources;
                                    self.resource_completions();
                                }
                                continue;
                            }
                            if route == "/commands" {
                                if let Ok(catalog) = serde_json::from_value::<
                                    eden_protocol::resources::CommandCatalog,
                                >(value.clone())
                                {
                                    self.commands = catalog.commands;
                                    self.resource_completions();
                                }
                                continue;
                            }
                            if route == "/queue/withdraw"
                                && let Ok(entries) = serde_json::from_value::<
                                    Vec<eden_protocol::coding::QueueEntry>,
                                >(value.clone())
                            {
                                for entry in entries {
                                    for reference in entry.references {
                                        if !self.references.iter().any(|r| r.id == reference.id) {
                                            self.references.push(reference.into());
                                        }
                                    }
                                    self.restore_blocks(
                                        entry.original_content.unwrap_or(entry.content),
                                    );
                                }
                            }
                            if matches!(route.as_str(), "/action" | "/private-input") {
                                let request = if route == "/private-input" {
                                    &body["request"]
                                } else {
                                    &body
                                };
                                let current = self.form_target.as_ref().is_some_and(|target| {
                                    request["owner"] == target.owner
                                        && request["view_id"] == target.view
                                        && if target.binding.is_some() {
                                            (if request["action"]
                                                == format!("{}:refresh", target.node)
                                            {
                                                self.form_retry.as_ref().is_some_and(
                                                    |(sent_route, sent, origin)| {
                                                        sent_route == &route
                                                            && sent == &body
                                                            && origin.binding == target.binding
                                                    },
                                                )
                                            } else {
                                                request["values"]["binding"]
                                                    == json!(target.binding)
                                            }) && request["action"]
                                                .as_str()
                                                .and_then(|action| action.rsplit_once(':'))
                                                .is_some_and(|(node, _)| node == target.node)
                                        } else {
                                            request["action"] == target.action
                                        }
                                });
                                if current
                                    && value["status"] == "applied"
                                    && let Some(Dialog::Form { fields, .. }) = &self.dialog
                                {
                                    self.form_applied = request["values"]["edits"]
                                        .as_array()
                                        .is_some_and(|edits| {
                                            edits.iter().all(|edit| {
                                                fields
                                                    .iter()
                                                    .find(|f| {
                                                        f.key == edit["path"].as_str().unwrap_or("")
                                                    })
                                                    .is_some_and(|f| edit_matches(f, edit))
                                            })
                                        });
                                }
                                if value["status"] == "applied" {
                                    if current
                                        && let Some(Dialog::Form { fields, .. }) = &mut self.dialog
                                    {
                                        acknowledge_edits(fields, request);
                                    }
                                    if let (Some(owner), Some(view), Some((node, _))) = (
                                        request["owner"].as_str(),
                                        request["view_id"].as_str(),
                                        request["action"].as_str().and_then(|a| a.rsplit_once(':')),
                                    ) {
                                        let key = format!("{owner}|{view}|{node}");
                                        if let Some((target, Dialog::Form { fields, .. })) =
                                            self.form_drafts.get_mut(&key)
                                            && json!(target.binding) == request["values"]["binding"]
                                        {
                                            acknowledge_edits(fields, request);
                                        }
                                    }
                                }
                                if current
                                    && request["action"]
                                        .as_str()
                                        .is_some_and(|a| a.ends_with(":refresh"))
                                {
                                    self.form_refresh = request["revision"].as_u64();
                                }
                                if self
                                    .form_retry
                                    .as_ref()
                                    .is_some_and(|(sent_route, sent, _)| {
                                        sent_route == &route && sent == &body
                                    })
                                {
                                    self.form_retry = None;
                                }
                                if current
                                    && let Some(Dialog::Form { status, .. }) = &mut self.dialog
                                {
                                    *status = value.to_string();
                                }
                            }
                            if let Some((_, _, draft)) = pending
                                && (!draft.text.is_empty()
                                    || !draft.attachments.is_empty()
                                    || !draft.references.is_empty())
                                && self.editor.text() == draft.text
                                && self.attachments == draft.attachments
                                && self.references == draft.references
                            {
                                let _ = self.editor.restore("", 0);
                                self.attachments.clear();
                                if matches!(route.as_str(), "/prompt" | "/enqueue") {
                                    self.references.clear();
                                }
                                self.changed();
                            }
                            if route == "/queue/inspect" {
                                self.queue = value
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .map(|v| {
                                        (
                                            v["id"].to_string(),
                                            format!("{} · {}", v["kind"], v["content"]),
                                        )
                                    })
                                    .collect();
                            }
                            self.notice = if route == "/cancel" {
                                "Cancellation accepted · waiting for cleanup".into()
                            } else {
                                format!("{route}: {}", text::clipped(&value.to_string(), 180))
                            };
                        }
                        Err(error) => {
                            if error.source == "live-client"
                                && matches!(
                                    error.code.as_str(),
                                    "Unavailable" | "InputFailure" | "OutputFailure"
                                )
                            {
                                if pending.is_some() {
                                    self.uncertain = pending;
                                }
                                self.notice = "Admission unknown · draft retained · Ctrl+R checks \
                                               original request"
                                    .into();
                            } else {
                                if self
                                    .form_retry
                                    .as_ref()
                                    .is_some_and(|(sent_route, sent, _)| {
                                        sent_route == &route && sent == &body
                                    })
                                {
                                    self.form_retry = None;
                                }
                                self.notice = error.to_string();
                            }
                        }
                    }
                }
            }
        }
        if !self.saved && self.edited_at.elapsed().as_millis() >= 150 {
            self.persist();
        }
        changed
    }
    pub fn retry(&mut self) {
        if let Some((route, body, _)) = &self.form_retry {
            self.dispatch(route, body.clone());
            return;
        }
        if let Some((_, body, _)) = &self.uncertain {
            self.dispatch(
                "/request-status",
                json!({ "request_id": body["request_id"] }),
            );
        } else {
            self.notice = "No uncertain submission".into();
        }
    }
    fn end_activity(&mut self) {
        if let Some((target, _)) = self.activity.take()
            && let Some(lease) = &self.lease
        {
            self.dispatch(
                "/activity",
                json!({
                    "attachment": lease.load(std::sync::atomic::Ordering::Relaxed),
                    "target": target,
                    "active": false,
                }),
            );
        }
    }
    pub fn open(&mut self, kind: &str) {
        self.clipboard_generation += 1;
        self.end_activity();
        self.autocomplete.dismiss();
        self.dialog_scroll = 0;
        self.dialog = Some(match kind {
            "help" => Dialog::Help,
            "style" => Dialog::Settings { selected: 0 },
            _ => Dialog::Palette {
                kind: kind.into(),
                query: String::new(),
                selected: 0,
            },
        });
        if kind == "queue" {
            self.dispatch("/queue/inspect", Value::Null)
        }
    }
    pub fn choices(&self, kind: &str, query: &str) -> Vec<(String, String)> {
        let all: Vec<(String, String)> = match kind {
            "search" => self
                .messages
                .iter()
                .filter(|m| m.body.to_lowercase().contains(&query.to_lowercase()))
                .map(|m| {
                    (
                        m.id.to_string(),
                        format!(
                            "{} · {}",
                            m.title,
                            text::clipped(&m.body.replace('\n', " "), 100)
                        ),
                    )
                })
                .collect(),
            "attachments" => self
                .attachments
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    (
                        i.to_string(),
                        format!("{} · {} bytes · snapshot", a.name, a.bytes.len()),
                    )
                })
                .collect(),
            kind if kind.starts_with("attachment:") => vec![
                ("refresh".into(), "Refresh snapshot from source file".into()),
                ("remove".into(), "Remove this attachment".into()),
            ],
            "queue" => self.queue.clone(),
            "resolve-request" => vec![
                (
                    "keep".into(),
                    "Keep draft and release uncertain request (do not send)".into(),
                ),
                (
                    "resend".into(),
                    "Resend original request · may duplicate an expired receipt".into(),
                ),
            ],
            "shells" => self
                .snapshot
                .state
                .shell_runs
                .iter()
                .map(|id| (id.to_string(), format!("Shell run {id} · Enter cancel")))
                .collect(),
            "artifacts" => crate::projection::artifacts(
                &self.snapshot.history,
                self.messages.get(self.selected).map_or(0, |m| m.id),
            )
            .into_iter()
            .map(|a| (a.path, format!("{} · {} bytes", a.name, a.bytes)))
            .collect(),
            "live" => self
                .snapshot
                .presentation
                .views
                .iter()
                .flat_map(|v| {
                    let mut items = vec![];
                    crate::forms::entries(&v.view.nodes, &mut items);
                    items
                        .into_iter()
                        .map(|(id, label)| {
                            (
                                format!("{}|{}|{id}", v.owner, v.view.id),
                                format!("{} / {} · {label}", v.owner, v.view.title),
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .collect(),
            _ => crate::autocomplete::commands()
                .into_iter()
                .map(|c| (c.label.clone(), format!("{}  {}", c.label, c.description)))
                .collect(),
        };
        if kind == "search" {
            all
        } else {
            all.into_iter()
                .filter(|(_, s)| s.to_lowercase().contains(&query.to_lowercase()))
                .collect()
        }
    }
    fn content(&self) -> std::result::Result<Vec<eden_protocol::coding::Block>, String> {
        use base64::Engine;
        use eden_protocol::coding::Block;
        let mut content = vec![Block::Text {
            text: self.editor.text(),
        }];
        for a in &self.attachments {
            let ext = Path::new(&a.name)
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            let media = a.media_type.as_deref().unwrap_or(match ext.as_str() {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                "pdf" => "application/pdf",
                "tif" | "tiff" => "image/tiff",
                "bmp" => "image/bmp",
                _ => "text/plain",
            });
            content.push(if media.starts_with("image/") {
                Block::Image {
                    media_type: media.into(),
                    data: base64::engine::general_purpose::STANDARD.encode(&a.bytes),
                }
            } else if media == "application/pdf" {
                Block::File {
                    name: a.name.clone(),
                    media_type: media.into(),
                    data: base64::engine::general_purpose::STANDARD.encode(&a.bytes),
                }
            } else {
                Block::Text {
                    text: format!(
                        "Attachment {}:\n{}",
                        a.name,
                        std::str::from_utf8(&a.bytes)
                            .map_err(|_| "Use a supported image/PDF or UTF-8 text attachment")?
                    ),
                }
            });
        }
        Ok(content)
    }
    pub fn submit(&mut self) {
        if self.connection != "Connected" {
            self.notice =
                "Disconnected · draft retained until the original session reconnects".into();
            return;
        }
        if self.attachment_pending > 0 {
            self.notice = "Capturing attachment content · draft retained".into();
            return;
        }
        let text = self.editor.text();
        if text.trim().is_empty() && self.attachments.is_empty() && self.references.is_empty() {
            return;
        }
        if self.command(text.trim()) {
            return;
        }
        if self.read_only {
            self.notice = "Saved history is read only".into();
            return;
        }
        if self.snapshot.state.active_run.is_some() {
            self.notice = "Session busy · draft retained · Alt+S steer / Alt+F follow-up".into();
            return;
        }
        match self.content() {
            Ok(content) => self.request("/prompt", json!({ "content": content }), true),
            Err(error) => self.notice = error,
        }
    }
    fn command(&mut self, text: &str) -> bool {
        if let Some(shell) = text.strip_prefix('!') {
            let excluded = shell.starts_with('!');
            let command = shell.strip_prefix('!').unwrap_or(shell);
            self.request(
                "/shell",
                json!({
                    "command": command,
                    "shell": if cfg!(windows) {
                            "powershell"
                        } else {
                            "bash"
                        },
                    "exclude_from_context": excluded,
                }),
                true,
            );
            return true;
        }
        if !text.starts_with('/') {
            return false;
        }
        let (name, args) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        match name {
            "/session" => self.open_references(None, false),
            "/references" => self.open_references(None, true),
            "/context" => self.open_context(),
            "/compact" => self.open_context_management(false),
            "/context-rebuild" => self.open_context_management(true),
            "/config" => self.dispatch("/configuration/open", json!({ "instance": args.trim() })),
            "/queue-mode" => {
                let mode = match args.trim() {
                    "one" => "one",
                    "all" => "all",
                    _ => {
                        self.notice = "Usage: /queue-mode one|all".into();
                        return true;
                    }
                };
                self.request(
                    "/queue/configure",
                    json!({ "steering": mode, "follow_up": mode }),
                    false,
                );
            }
            "/shell" => {
                self.request(
                    "/shell",
                    json!({
                        "command": args,
                        "shell": if cfg!(windows) {
                                "powershell"
                            } else {
                                "bash"
                            },
                        "exclude_from_context": false,
                    }),
                    true,
                );
                return true;
            }
            "/quit" => self.quit = true,
            "/stop-host" => self.request("/shutdown", json!({}), false),
            "/recover" => self.restore_recovery(),
            "/inspect" => self.preferences.inspector = !self.preferences.inspector,
            "/copy" => self.copy_last(),
            "/attach" => {
                let path = PathBuf::from(args.trim().trim_matches('"'));
                if let Err(error) = self.attach(&path) {
                    self.notice = error.to_string();
                }
            }
            "/live" => self.open("live"),
            "/help" | "/style" | "/search" | "/commands" | "/attachments" | "/queue"
            | "/shells" | "/artifacts" => self.open(name.trim_start_matches('/')),
            _ => {
                let resource = name.trim_start_matches('/');
                if self.resources.templates.iter().any(|r| r.name == resource)
                    || resource
                        .strip_prefix("skill:")
                        .is_some_and(|name| self.resources.skills.iter().any(|r| r.name == name))
                {
                    return false;
                }
                if self.commands.iter().any(|c| c.name == resource) {
                    match serde_json::from_str::<Value>(if args.trim().is_empty() {
                        "{}"
                    } else {
                        args
                    }) {
                        Ok(arguments) => self.request(
                            "/command",
                            json!({ "name": resource, "arguments": arguments }),
                            true,
                        ),
                        Err(error) => {
                            self.notice = format!("Command arguments must be JSON: {error}")
                        }
                    }
                    return true;
                }
                self.notice = format!("Unknown command {name}");
                return true;
            }
        }
        if !matches!(name, "/recover" | "/attach") {
            let _ = self.editor.restore("", 0);
            self.changed();
        }
        true
    }
    pub fn enqueue(&mut self, kind: &str, _text: &str) {
        match self.content() {
            Ok(content) => self.request(
                "/enqueue",
                json!({
                    "kind": if kind == "steer" {
                            "steering"
                        } else {
                            "follow_up"
                        },
                    "content": content,
                }),
                true,
            ),
            Err(error) => self.notice = error,
        }
    }
    pub fn cancel(&mut self) {
        if self.preferences.inspector {
            self.preferences.inspector = false;
            self.focus = Focus::Editor;
            return;
        }
        if let Some(run) = self.snapshot.state.active_run {
            self.dispatch("/cancel", json!({ "run_id": run }));
            self.phase = Phase::Cancelling;
        }
    }
    pub fn attach(&mut self, path: &Path) -> Result<()> {
        let root = Path::new(&self.snapshot.state.cwd);
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            root.join(path)
        };
        let tx = self.tx.clone();
        let generation = self.attachment_generation;
        self.attachment_pending += 1;
        self.runtime.spawn_blocking(move || {
            let result = read_attachment(&path);
            let _ = tx.send(Update::Attachment { generation, result });
        });
        Ok(())
    }
    pub fn copy(&mut self) {
        let editor = self.editor.selected_text();
        let content = if !editor.is_empty() {
            editor
        } else {
            self.selection
                .map(|s| s.text(&self.rows))
                .unwrap_or_default()
        };
        if !content.is_empty() {
            self.copied = Some(content)
        }
    }
    fn copy_last(&mut self) {
        self.copied = self
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .map(|m| m.body.clone());
    }
    pub fn scroll(&mut self, delta: isize) {
        if self.focus == Focus::Inspector {
            self.inspector_scroll = self.inspector_scroll.saturating_add_signed(delta);
            return;
        }
        self.top = self
            .top
            .saturating_add_signed(delta)
            .min(self.rows.len().saturating_sub(self.viewport));
        self.follow = self.top == self.rows.len().saturating_sub(self.viewport);
        if let Some(row) = self.rows.get(self.top) {
            self.anchor = row.anchor;
        }
        if self.follow {
            self.unseen = 0;
        }
    }
    pub fn bottom(&mut self) {
        self.follow = true;
        self.unseen = 0;
        self.focus = Focus::Editor;
    }
    pub fn toggle_message(&mut self) {
        if let Some(m) = self.messages.get_mut(self.selected) {
            m.expanded = !m.expanded;
            self.inspector_scroll = 0;
        }
    }
    fn undo_draft(&mut self, redo: bool) {
        let previous = if redo {
            self.redo.pop()
        } else {
            self.undo.pop()
        };
        let Some(previous) = previous else { return };
        let current = self.draft();
        if previous.text != current.text {
            let _ = self.editor.event(0, if redo { 13 } else { 12 }, 0, "");
            if self.editor.text() != previous.text {
                let _ = self.editor.restore(&previous.text, previous.cursor);
            }
        }
        self.attachments = previous.attachments;
        self.references = previous.references;
        self.checkpoint = self.draft();
        if redo {
            self.undo.push(current)
        } else {
            self.redo.push(current)
        }
        self.saved = false;
        self.edited_at = Instant::now();
        self.sync_completion();
    }
    fn history_move(&mut self, delta: isize) {
        if self.input_history.is_empty() {
            return;
        }
        if self.history_position.is_none() {
            if delta > 0 {
                return;
            }
            self.history_draft = self.draft();
            self.history_position = Some(self.input_history.len());
        }
        let index = self
            .history_position
            .unwrap_or(0)
            .saturating_add_signed(delta)
            .min(self.input_history.len());
        if index == self.input_history.len() {
            let draft = self.history_draft.clone();
            let _ = self.editor.restore(&draft.text, draft.cursor);
            self.attachments = draft.attachments;
            self.references = draft.references;
            self.history_position = None;
        } else {
            let text = &self.input_history[index];
            let _ = self.editor.restore(text, text.len());
            self.attachments.clear();
            self.references.clear();
            self.history_position = Some(index);
        }
        self.changed();
    }
    pub fn sync_completion(&mut self) {
        self.autocomplete.refresh(
            &self.editor.text(),
            self.editor.cursor(),
            self.focus == Focus::Editor && self.dialog.is_none(),
        );
    }
    pub fn accept_completion(&mut self, index: usize) {
        if let Some((replacement, item)) =
            self.autocomplete
                .take(index, &self.editor.text(), self.editor.cursor())
        {
            if item.action == crate::autocomplete::Action::Session {
                self.open_references(Some(replacement), false);
                return;
            }
            if let crate::autocomplete::Action::File(path) = item.action {
                let tx = self.tx.clone();
                self.attachment_pending += 1;
                let source = self.editor.text();
                let cursor = self.editor.cursor();
                let generation = self.attachment_generation;
                self.runtime.spawn_blocking(move || {
                    let result = read_attachment(&path);
                    let _ = tx.send(Update::Completion {
                        replacement,
                        insert: item.insert,
                        source,
                        cursor,
                        generation,
                        result,
                    });
                });
                return;
            }
            if replacement
                .apply(&self.editor.text(), &item.insert)
                .is_some()
            {
                let _ = self
                    .editor
                    .replace_range(replacement.token.range, &item.insert);
                self.changed();
            }
            self.sync_completion();
        }
    }
    pub fn paste(&mut self, text: &str) {
        if self.reference_picker.open {
            return;
        }
        if self.context.open {
            if self.context.editing.is_some() {
                let _ = self.field_editor.event(1, 0, 0, text);
            }
            return;
        }
        self.clipboard_generation += 1;
        if let Some(Dialog::Form {
            fields, selected, ..
        }) = &mut self.dialog
        {
            if let Some(field) = fields.get_mut(*selected) {
                let _ = field.paste(text);
            }
        } else if self.dialog.is_none() {
            let _ = self.editor.event(1, 0, 0, text);
            self.changed();
            self.sync_completion();
        }
    }
    pub fn form_click(&mut self, index: usize) {
        if let Some(Dialog::Form {
            selected, fields, ..
        }) = &mut self.dialog
        {
            *selected = index.min(fields.len().saturating_sub(1));
            if let Some(field) = fields.get_mut(*selected)
                && !field.is_text()
            {
                field.activate();
            }
        }
    }
    pub fn form_choice(&mut self, index: usize) {
        if let Some(Dialog::Form {
            selected, fields, ..
        }) = &mut self.dialog
            && let Some(field) = fields.get_mut(*selected)
        {
            field.choose(index);
        }
    }
    pub fn dialog_key(&mut self, key: KeyEvent) {
        self.clipboard_generation += 1;
        if key.code == KeyCode::Esc {
            if let Some(mut dialog) = self.dialog.take()
                && let Dialog::Form { fields, .. } = &mut dialog
            {
                for field in fields.iter_mut().filter(|f| f.private) {
                    field.value.clear();
                    field.initial.clear();
                }
                if let Some(target) = &self.form_target {
                    self.form_drafts.insert(
                        format!("{}|{}|{}", target.owner, target.view, target.node),
                        (target.clone(), dialog),
                    );
                }
            }
            return;
        }
        if matches!(self.dialog, Some(Dialog::Form { .. }))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            if key.code == KeyCode::Char('d') {
                if let Some(target) = &self.form_target {
                    let id = format!("{}|{}|{}", target.owner, target.view, target.node);
                    self.form_drafts.remove(&id);
                    self.open_live(&id);
                }
                return;
            }
            if key.code == KeyCode::Char('b') {
                if let Some(Dialog::Form {
                    fields, selected, ..
                }) = &mut self.dialog
                    && let Some(field) = fields.get_mut(*selected)
                {
                    field.inherit = true;
                    field.clear = false;
                }
                return;
            }
            let action = match key.code {
                KeyCode::Char('s') => Some("apply"),
                KeyCode::Char('v') => Some("validate"),
                KeyCode::Char('p') => Some("preview"),
                KeyCode::Char('k') => Some("cancel_apply"),
                KeyCode::Char('f') => Some("refresh"),
                _ => None,
            };
            if let Some(action) = action {
                self.submit_form(action);
                return;
            }
        }
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        match &mut dialog {
            Dialog::Help => {
                if key.code == KeyCode::PageDown {
                    self.dialog_scroll += 10
                } else if key.code == KeyCode::PageUp {
                    self.dialog_scroll = self.dialog_scroll.saturating_sub(10)
                }
            }
            Dialog::Settings { selected } => match key.code {
                KeyCode::Up => *selected = selected.saturating_sub(1),
                KeyCode::Down => *selected = (*selected + 1).min(8),
                KeyCode::Tab => *selected = (*selected / 3 + 1) % 3 * 3,
                KeyCode::Char(' ') | KeyCode::Enter => match *selected {
                    0 => self.preferences.light = !self.preferences.light,
                    1 => self.preferences.compact = !self.preferences.compact,
                    2 => self.preferences.basic = !self.preferences.basic,
                    3 => self.preferences.diff_split = !self.preferences.diff_split,
                    4 => self.preferences.thinking = !self.preferences.thinking,
                    5 => self.preferences.footer = !self.preferences.footer,
                    6 => self.preferences.mouse = !self.preferences.mouse,
                    7 => self.preferences.motion = !self.preferences.motion,
                    _ => self.editor.style = 1 - self.editor.style,
                },
                _ => {}
            },
            Dialog::Palette {
                kind,
                query,
                selected,
            } => {
                let choices = self.choices(kind, query);
                match key.code {
                    KeyCode::Up => *selected = selected.saturating_sub(1),
                    KeyCode::Down => {
                        *selected = (*selected + 1).min(choices.len().saturating_sub(1))
                    }
                    KeyCode::PageDown => {
                        *selected = (*selected + 8).min(choices.len().saturating_sub(1))
                    }
                    KeyCode::PageUp => *selected = selected.saturating_sub(8),
                    KeyCode::Backspace => {
                        pop_grapheme(query);
                        *selected = 0
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        query.push(c);
                        *selected = 0
                    }
                    KeyCode::Enter => {
                        if let Some((id, _)) = choices.get(*selected) {
                            self.choose(kind, id);
                            return;
                        }
                    }
                    _ => {}
                }
            }
            Dialog::Form {
                fields,
                selected,
                status,
                ..
            } => {
                if key.code == KeyCode::Tab {
                    *selected = (*selected + 1) % fields.len().max(1)
                } else if let Some(field) = fields.get_mut(*selected)
                    && let Err(error) = field.handle_key(key)
                {
                    *status = error;
                }
            }
        }
        self.dialog = Some(dialog);
    }
    fn choose(&mut self, kind: &str, id: &str) {
        match kind {
            "resolve-request" => {
                if self.uncertain_checked {
                    if id == "keep" {
                        self.uncertain = None;
                        self.saved = false;
                    } else if let Some((route, body, draft)) = self.uncertain.take() {
                        self.pending = Some((route.clone(), body.clone(), draft));
                        self.dispatch(&route, body);
                    }
                    self.uncertain_checked = false;
                }
            }
            "search" => {
                if let Ok(record) = id.parse() {
                    self.follow = false;
                    self.anchor = Anchor {
                        record,
                        line: 0,
                        byte: 0,
                    };
                    self.selected = self
                        .messages
                        .iter()
                        .position(|m| m.id == record)
                        .unwrap_or(0);
                }
            }
            "live" => self.open_live(id),
            "attachments" => self.open(&format!("attachment:{id}")),
            kind if kind.starts_with("attachment:") => {
                if let Some(index) = kind
                    .strip_prefix("attachment:")
                    .and_then(|s| s.parse::<usize>().ok())
                    && let Some(before) = self.attachments.get(index).cloned()
                {
                    if id == "remove" {
                        self.attachments.remove(index);
                        self.changed();
                    } else if before.source.is_empty() {
                        self.notice = "This snapshot has no file source to refresh".into();
                    } else {
                        let path = PathBuf::from(&before.source);
                        let tx = self.tx.clone();
                        self.runtime.spawn_blocking(move || {
                            let result = read_attachment(&path);
                            let _ = tx.send(Update::Refreshed {
                                index,
                                before,
                                result,
                            });
                        });
                    }
                }
            }
            "queue" => {
                if let Ok(id) = id.parse::<u64>() {
                    self.request("/queue/withdraw", json!({ "ids": [id] }), false);
                }
            }
            "shells" => {
                if let Ok(id) = id.parse::<u64>() {
                    self.dispatch("/shell/cancel", json!({ "run_id": id }));
                }
            }
            "artifacts" => {
                if let Some(message) = self.messages.get(self.selected) {
                    let record = message.id;
                    let path = id.to_owned();
                    let tx = self.tx.clone();
                    self.runtime.spawn_blocking(move || {
                        let result = std::fs::read_to_string(path).map_err(|e| e.to_string());
                        let _ = tx.send(Update::Artifact { record, result });
                    });
                }
            }
            _ => {
                self.command(id);
            }
        }
    }
    fn resource_completions(&mut self) {
        use crate::autocomplete::{Action, Candidate};
        self.autocomplete.extra = self
            .resources
            .skills
            .iter()
            .map(|r| (format!("skill:{}", r.name), r.description.clone()))
            .chain(
                self.resources
                    .templates
                    .iter()
                    .map(|r| (r.name.clone(), r.description.clone())),
            )
            .chain(
                self.commands
                    .iter()
                    .map(|c| (c.name.clone(), c.description.clone())),
            )
            .map(|(name, description)| Candidate {
                label: format!("/{name}"),
                description,
                insert: format!("/{name} "),
                action: Action::Command,
            })
            .collect();
    }
    fn restore_blocks(&mut self, content: Vec<eden_protocol::coding::Block>) {
        use base64::Engine;
        use eden_protocol::coding::Block;
        for block in content {
            match block {
                Block::Text { text } => {
                    let previous = self.editor.text();
                    let joined = if previous.is_empty() {
                        text
                    } else {
                        format!("{previous}\n{text}")
                    };
                    let _ = self.editor.restore(&joined, joined.len());
                }
                Block::Image { media_type, data } => {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                        self.attachments.push(Attachment {
                            name: format!(
                                "queued.{}",
                                media_type.split('/').nth(1).unwrap_or("png")
                            ),
                            source: String::new(),
                            bytes: bytes.into(),
                            media_type: Some(media_type),
                            image: true,
                        });
                    }
                }
                Block::File { name, data, .. } => {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                        self.attachments.push(Attachment {
                            name,
                            source: String::new(),
                            bytes: bytes.into(),
                            media_type: None,
                            image: false,
                        });
                    }
                }
            }
        }
        self.changed();
    }
    fn open_live(&mut self, id: &str) {
        self.clipboard_generation += 1;
        self.end_activity();
        let parts: Vec<_> = id.split('|').collect();
        if parts.len() != 3 {
            return;
        }
        let Some(view) = self
            .snapshot
            .presentation
            .views
            .iter()
            .find(|v| v.owner == parts[0] && v.view.id == parts[1])
        else {
            return;
        };
        if !view.active {
            self.notice = "View settled · actions unavailable".into();
            return;
        }
        if let Some((target, fields)) = crate::forms::find(view, parts[2]) {
            let saved = self
                .form_drafts
                .remove(id)
                .filter(|(_, dialog)| match dialog {
                    Dialog::Form { fields: old, .. } => old.iter().any(|old| {
                        !old.private
                            && (old.clear
                                || old.inherit
                                || (old.value != old.initial
                                    && fields
                                        .iter()
                                        .find(|field| field.key == old.key)
                                        .is_none_or(|field| field.value != old.value)))
                    }),
                    _ => false,
                });
            let (target, dialog) = saved.unwrap_or((
                target,
                Dialog::Form {
                    title: view.view.title.clone(),
                    fields,
                    selected: 0,
                    status: "Ctrl+V validate · Ctrl+P preview · Ctrl+S apply · Ctrl+K cancel · \
                             Ctrl+D discard · Ctrl+B inherit · Esc back"
                        .into(),
                },
            ));
            self.form_applied = false;
            self.form_refresh = None;
            self.form_target = Some(target);
            self.dialog = Some(dialog);
        } else {
            fn button(nodes: &[eden_protocol::presentation::Node], id: &str) -> Option<String> {
                for n in nodes {
                    match n {
                        eden_protocol::presentation::Node::Button {
                            id: found, action, ..
                        } if found == id => return Some(action.clone()),
                        eden_protocol::presentation::Node::Group { children, .. } => {
                            if let Some(action) = button(children, id) {
                                return Some(action);
                            }
                        }
                        _ => {}
                    }
                }
                None
            }
            if let Some(action) = button(&view.view.nodes, parts[2]) {
                self.dispatch(
                    "/action",
                    json!({
                        "session_id": self.snapshot.presentation.session_id,
                        "owner": view.owner,
                        "view_id": view.view.id,
                        "revision": view.revision,
                        "action": action,
                        "request_id": self.id(),
                        "values": {},
                    }),
                );
            }
        }
    }
    fn rebind_applied_form(&mut self) {
        if !self.form_applied && self.form_refresh.is_none() {
            return;
        }
        let Some(target) = &self.form_target else {
            return;
        };
        if let Some(view) = self
            .snapshot
            .presentation
            .views
            .iter()
            .find(|v| v.owner == target.owner && v.view.id == target.view)
            && let Some((latest, mut fields)) = crate::forms::find(view, &target.node)
            && (latest.binding != target.binding || latest.revision != target.revision)
            && self
                .form_refresh
                .is_none_or(|submitted| latest.revision > submitted)
        {
            if let Some(Dialog::Form {
                fields: current,
                selected,
                status,
                ..
            }) = &mut self.dialog
            {
                {
                    for field in &mut fields {
                        if let Some(old) = current.iter().find(|f| f.key == field.key)
                            && !old.private
                            && (old.value != old.initial || old.clear || old.inherit)
                        {
                            field.value = old.value.clone();
                            field.clear = old.clear;
                            field.inherit = old.inherit;
                            field.cursor = old.cursor.min(field.value.len());
                        }
                    }
                }
                *current = fields;
                *selected = (*selected).min(current.len().saturating_sub(1));
                *status = if self.form_refresh.is_some() {
                    "Refreshed · draft retained; validate and preview before applying"
                } else {
                    "Applied · editing current configuration"
                }
                .into();
            }
            self.form_target = Some(latest);
            self.form_applied = false;
            self.form_refresh = None;
        }
    }
    fn submit_form(&mut self, action: &str) {
        self.rebind_applied_form();
        if self.form_retry.is_some() {
            self.notice = "Form request pending · Ctrl+R recovers the original request".into();
            return;
        }

        let Some(target) = &self.form_target else {
            return;
        };
        let Some(Dialog::Form { fields, .. }) = &self.dialog else {
            return;
        };
        let current = self
            .snapshot
            .presentation
            .views
            .iter()
            .find(|v| v.owner == target.owner && v.view.id == target.view);
        let Some((latest, _)) = current.and_then(|v| crate::forms::find(v, &target.node)) else {
            self.notice = "Configuration unavailable · draft preserved".into();
            return;
        };
        if action != "refresh" && latest.binding != target.binding {
            self.notice = "Configuration changed · draft preserved; refresh to review".into();
            return;
        }
        // Refresh reads current authority; Apply continues to compare the draft's binding.
        let request_target = if action == "refresh" {
            latest
        } else {
            crate::forms::Target {
                revision: latest.revision,
                ..target.clone()
            }
        };
        let (request, private) = match crate::forms::submission(
            &request_target,
            if action == "refresh" { &[] } else { fields },
            self.snapshot.presentation.session_id,
            &self.id(),
            action,
        ) {
            Ok(value) => value,
            Err(error) => {
                self.notice = error;
                return;
            }
        };
        let route = if private.is_empty() {
            "/action"
        } else {
            "/private-input"
        };
        let public = if private.is_empty() {
            request.clone()
        } else {
            json!({ "request": request, "inputs": [] })
        };
        self.form_retry = Some((route.into(), public.clone(), target.clone()));
        let body = if private.is_empty() {
            request
        } else {
            json!({ "request": request, "inputs": private })
        };
        let endpoint = self.endpoint.clone();
        let tx = self.tx.clone();
        let route = route.to_owned();
        self.runtime.spawn(async move {
            let result = eden_tui_client::call(&endpoint, "POST", &route, Some(&body)).await;
            drop(body);
            let _ = tx.send(Update::Reply {
                route,
                body: public,
                result,
            });
        });
        if let Some(Dialog::Form { fields, .. }) = &mut self.dialog {
            for field in fields.iter_mut().filter(|f| f.private) {
                field.value.clear();
                field.initial.clear();
            }
        }
    }
}

impl App {
    pub fn key(&mut self, key: KeyEvent) {
        if self.reference_picker.open {
            if key.kind != KeyEventKind::Release {
                self.reference_key(key);
            }
            return;
        }
        if self.context.open {
            if key.kind != KeyEventKind::Release {
                self.context_key(key);
            }
            return;
        }
        if key.kind != KeyEventKind::Release {
            self.clipboard_generation += 1;
        }
        if !matches!(key.code, KeyCode::Up | KeyCode::Down) {
            self.history_position = None;
        }
        if key.kind != KeyEventKind::Release && self.dialog.is_none() && self.focus == Focus::Editor
        {
            if matches!(key.code, KeyCode::Char('z' | '-'))
                && key.modifiers == KeyModifiers::CONTROL
                && !self
                    .preferences
                    .bindings
                    .contains_key(if key.code == KeyCode::Char('-') {
                        "composer.ctrl+-"
                    } else {
                        "composer.ctrl+z"
                    })
            {
                self.undo_draft(false);
                return;
            }
            if key.code == KeyCode::Char('z')
                && key.modifiers == KeyModifiers::ALT
                && !self.preferences.bindings.contains_key("composer.alt+z")
            {
                self.undo_draft(true);
                return;
            }
            if key.code == KeyCode::Up
                && key.modifiers.is_empty()
                && (self.history_position.is_some()
                    || self.editor.cursor() == 0
                    || self.editor.text().is_empty())
            {
                self.history_move(-1);
                return;
            }
            if key.code == KeyCode::Down
                && key.modifiers.is_empty()
                && self.history_position.is_some()
            {
                self.history_move(1);
                return;
            }
        }

        if key.kind != KeyEventKind::Release {
            let context = if self.dialog.is_some() {
                "modal"
            } else {
                match self.focus {
                    Focus::Editor => "composer",
                    Focus::Transcript => "transcript",
                    Focus::Inspector => "inspector",
                }
            };
            let name = match key.code {
                KeyCode::Char(c) => c.to_ascii_lowercase().to_string(),
                KeyCode::Enter => "enter".into(),
                KeyCode::Esc => "escape".into(),
                KeyCode::Tab => "tab".into(),
                KeyCode::F(n) => format!("f{n}"),
                _ => String::new(),
            };
            let binding = format!(
                "{context}.{}{}{}{name}",
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    "ctrl+"
                } else {
                    ""
                },
                if key.modifiers.contains(KeyModifiers::ALT) {
                    "alt+"
                } else {
                    ""
                },
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    "shift+"
                } else {
                    ""
                }
            );
            if let Some(action) = self.preferences.bindings.get(&binding).cloned() {
                if key.kind == KeyEventKind::Repeat {
                    return;
                }
                match action.as_str() {
                    "send" => self.submit(),
                    "cancel" => self.cancel(),
                    "copy" => self.copy(),
                    "clear" => {
                        let _ = self.editor.restore("", 0);
                        self.attachments.clear();
                        self.references.clear();
                        self.changed();
                    }
                    "external_editor" => self.external_editor = true,
                    "quit" => self.quit = true,
                    "search" => self.open("search"),
                    "inspector" => self.preferences.inspector = !self.preferences.inspector,
                    "navigator" => self.preferences.navigator = !self.preferences.navigator,
                    "settings" => self.open("style"),
                    "steering" => self.enqueue("steer", ""),
                    "follow_up" => self.enqueue("follow", ""),
                    "paste" => self.clipboard_read = true,
                    "undo" => self.undo_draft(false),
                    "redo" => self.undo_draft(true),
                    "close" => self.dialog = None,
                    _ => {}
                }
                return;
            }
        }

        if key.kind == KeyEventKind::Repeat && key.code == KeyCode::Enter {
            return;
        }

        if key.kind == KeyEventKind::Release {
            return;
        }
        if self.dialog.is_none()
            && self.focus == Focus::Editor
            && self.autocomplete.open
            && key.modifiers == KeyModifiers::NONE
        {
            match key.code {
                KeyCode::Up => {
                    self.autocomplete.move_selection(-1);
                    return;
                }
                KeyCode::Down => {
                    self.autocomplete.move_selection(1);
                    return;
                }
                KeyCode::PageUp => {
                    self.autocomplete.move_selection(-6);
                    return;
                }
                KeyCode::PageDown => {
                    self.autocomplete.move_selection(6);
                    return;
                }
                KeyCode::Esc => {
                    self.autocomplete.dismiss();
                    return;
                }
                KeyCode::Enter
                    if self
                        .autocomplete
                        .items
                        .get(self.autocomplete.selected)
                        .is_some_and(|item| {
                            item.action == crate::autocomplete::Action::Command
                                && item.label == self.editor.text().trim()
                        }) =>
                {
                    self.autocomplete.dismiss();
                    self.handle_key(key);
                    self.sync_completion();
                    return;
                }
                KeyCode::Tab | KeyCode::Enter if !self.autocomplete.items.is_empty() => {
                    self.accept_completion(self.autocomplete.selected);
                    return;
                }
                _ => {}
            }
        }
        self.handle_key(key);
        self.sync_completion();
    }
    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if self.dialog.is_some() {
            self.dialog_key(key);
            return;
        }
        if let KeyCode::F(n) = key.code {
            match n {
                1 => self.open("help"),
                2 => self.open("commands"),
                3 => self.preferences.inspector = !self.preferences.inspector,
                4 => self.preferences.navigator = !self.preferences.navigator,
                7 => self.open_context(),

                6 => {
                    self.focus = match self.focus {
                        Focus::Editor => Focus::Transcript,
                        Focus::Transcript => {
                            if self.preferences.inspector {
                                Focus::Inspector
                            } else {
                                Focus::Editor
                            }
                        }
                        Focus::Inspector => Focus::Editor,
                    };
                }

                8 => self.bottom(),
                9 => self.preferences.light = !self.preferences.light,
                10 => self.open("style"),

                12 => {
                    let _ = self.editor.switch();
                    self.notice =
                        "Native editor switched · draft, selection and undo transferred".into();
                }
                _ => {}
            }
            return;
        }
        if self.dialog.is_some() {
            self.dialog_key(key);
            return;
        }
        match key.code {
            KeyCode::Esc => self.cancel(),
            KeyCode::Char('d')
                if ctrl && self.editor.text().is_empty() && self.references.is_empty() =>
            {
                self.quit = true
            }
            KeyCode::Char('c') if ctrl => {
                if !self.editor.selected_text().is_empty() || self.selection.is_some() {
                    self.copy();
                } else {
                    let _ = self.editor.restore("", 0);
                    self.attachments.clear();
                    self.references.clear();
                    self.attachment_generation += 1;
                    self.attachment_pending = 0;
                    self.changed();
                    self.notice = "Draft cleared · task remains active".into();
                }
            }
            KeyCode::Char('g') if ctrl => self.external_editor = true,
            KeyCode::Char('v') if ctrl => self.clipboard_read = true,
            KeyCode::Char('o') if ctrl => self.toggle_message(),
            KeyCode::Char('t') if ctrl => self.preferences.thinking = !self.preferences.thinking,
            KeyCode::Char('f') if ctrl => self.open("search"),
            KeyCode::Char('z') if ctrl && alt => self.suspend = true,
            KeyCode::Char('r') if ctrl => self.retry(),
            KeyCode::Char('s') if alt => self.enqueue("steer", ""),
            KeyCode::Char('f') if alt => self.enqueue("follow", ""),
            KeyCode::Char('a') if alt => self.open("attachments"),
            KeyCode::PageUp => self.scroll(-(self.viewport as isize)),
            KeyCode::PageDown => self.scroll(self.viewport as isize),
            KeyCode::End if ctrl || self.focus != Focus::Editor => self.bottom(),
            KeyCode::Home if self.focus != Focus::Editor => {
                self.top = 0;
                self.follow = false;
                self.anchor = self.rows.first().map_or(Anchor::default(), |r| r.anchor);
            }
            KeyCode::Tab if self.focus == Focus::Editor => {
                self.autocomplete
                    .force(&self.editor.text(), self.editor.cursor());
                if !self.autocomplete.open {
                    self.open("commands");
                }
            }
            KeyCode::Up | KeyCode::Down if self.focus != Focus::Editor => {
                let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                if self.focus == Focus::Inspector {
                    self.inspector_scroll = self.inspector_scroll.saturating_add_signed(delta);
                } else if key.modifiers.contains(KeyModifiers::SHIFT) {
                    if let Some(row) = self.rows.get(self.top) {
                        let mut selection = self
                            .selection
                            .unwrap_or_else(|| crate::selection::Selection::from_row(row, 0));
                        self.scroll(delta);
                        if let Some(end) = self.rows.get(self.top) {
                            selection.extend(end, usize::MAX);
                            self.selection = Some(selection);
                        }
                    }
                } else {
                    self.selected = self
                        .selected
                        .saturating_add_signed(delta)
                        .min(self.messages.len().saturating_sub(1));
                    self.follow = false;
                    if let Some(m) = self.messages.get(self.selected) {
                        self.anchor = Anchor {
                            record: m.id,
                            line: 0,
                            byte: 0,
                        };
                    }
                }
            }
            KeyCode::Enter if self.focus != Focus::Editor => self.toggle_message(),
            KeyCode::Enter if key.kind == KeyEventKind::Repeat => {}
            KeyCode::Enter
                if !key.modifiers.intersects(
                    KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL,
                ) =>
            {
                self.submit()
            }
            _ => {
                if self.focus == Focus::Editor {
                    send_editor_key(&mut self.editor, key);
                    self.changed();
                }
            }
        }
    }
}
pub fn send_editor_key(editor: &mut Editor, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mods = u32::from(key.modifiers.contains(KeyModifiers::SHIFT))
        | if ctrl { 2 } else { 0 }
        | if alt { 4 } else { 0 };
    let mut text = String::new();
    let code = match key.code {
        KeyCode::Left => {
            if ctrl || alt {
                17
            } else {
                1
            }
        }
        KeyCode::Right => {
            if ctrl || alt {
                18
            } else {
                2
            }
        }
        KeyCode::Up => 3,
        KeyCode::Down => 4,
        KeyCode::Home => 5,
        KeyCode::End => 6,
        KeyCode::Backspace => {
            if ctrl || alt {
                14
            } else {
                7
            }
        }
        KeyCode::Delete => 8,
        KeyCode::Enter => 9,
        KeyCode::Char('a') if ctrl => 11,
        KeyCode::Char('z' | '-') if ctrl => 12,
        KeyCode::Char('y') if ctrl => 16,
        KeyCode::Char('z') if alt => 13,
        KeyCode::Char('y') if alt => 20,
        KeyCode::Char('w') if ctrl => 14,
        KeyCode::Char('k') if ctrl => 15,
        KeyCode::Char('u') if ctrl => 19,
        KeyCode::Char('j') if ctrl => 9,
        KeyCode::Char(c) if !ctrl && !alt => {
            text.push(c);
            10
        }
        _ => return,
    };
    let _ = editor.event(0, code, mods, &text);
}
fn pop_grapheme(text: &mut String) {
    use unicode_segmentation::UnicodeSegmentation;
    if let Some((i, _)) = text.grapheme_indices(true).next_back() {
        text.truncate(i);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ratatui::{buffer::Buffer, layout::Rect};
    pub(crate) fn app() -> App {
        let executable = std::env::current_exe().unwrap();
        let plugin = executable.parent().unwrap().parent().unwrap().join(format!(
            "{}eden_terminal_editor{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX
        ));
        let snapshot: Snapshot = serde_json::from_value(json!({
            "presentation": {
                "version": eden_protocol::presentation::VERSION,
                "session_id": 7,
                "sequence": 0,
                "views": [],
                "activity": [],
                "pending_interactions": [],
            },
            "state": { "session_id": 7, "cwd": ".", "closed": false, "active_run": null },
            "history": [],
            "events": [],
        }))
        .unwrap();
        let (tx, rx) = mpsc::channel();
        App::new(
            &plugin,
            None,
            "unit-test",
            Path::new("/not-an-endpoint"),
            snapshot,
            tx,
            rx,
        )
        .unwrap()
    }
    #[tokio::test]
    async fn busy_send_keeps_text_and_attachment_snapshot() {
        let mut app = app();
        app.snapshot.state.active_run = Some(4);
        app.editor.restore("next task", 9).unwrap();
        app.attachments.push(Attachment {
            name: "note.txt".into(),
            source: "removed".into(),
            bytes: b"snapshot".to_vec().into(),
            media_type: None,
            image: false,
        });
        let before = app.draft();
        app.submit();
        assert_eq!(app.draft(), before);
        assert!(app.pending.is_none());
        assert!(app.notice.contains("busy"));
    }
    #[tokio::test]
    async fn pasted_commands_and_repeated_enter_never_submit() {
        let mut app = app();
        app.paste("/quit\n!echo no\n中文🙂");
        assert!(!app.quit);
        assert!(app.pending.is_none());
        assert_eq!(app.editor.text(), "/quit\n!echo no\n中文🙂");
        let mut key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        key.kind = KeyEventKind::Repeat;
        app.key(key);
        assert!(app.pending.is_none());
        assert!(!app.quit);
    }
    #[tokio::test]
    async fn modal_consumes_global_keys_and_escape_before_cancel() {
        let mut app = app();
        app.snapshot.state.active_run = Some(4);
        app.open("help");
        app.key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE));
        assert!(!app.preferences.inspector);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.dialog.is_none());
        assert_ne!(app.phase, Phase::Cancelling);
    }
    #[tokio::test]
    async fn control_c_clears_without_cancelling_or_copying() {
        let mut app = app();
        app.snapshot.state.active_run = Some(4);
        app.editor.restore("draft", 5).unwrap();
        app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.editor.text().is_empty());
        assert!(app.copied.is_none());
        assert_ne!(app.phase, Phase::Cancelling);
    }
    #[tokio::test]
    async fn reply_preserves_new_edits_and_unrelated_cancel_keeps_pending() {
        let mut app = app();
        app.editor.restore("original", 8).unwrap();
        let draft = app.draft();
        let body = json!({ "request_id": "same", "content": [] });
        app.pending = Some(("/prompt".into(), body.clone(), draft));
        app.editor.restore("edited while sending", 20).unwrap();
        app.tx
            .send(Update::Reply {
                route: "/cancel".into(),
                body: json!({ "run_id": 1 }),
                result: Ok(json!({ "cancel_requested": true })),
            })
            .unwrap();
        app.tick();
        assert!(app.pending.is_some());
        app.tx
            .send(Update::Reply {
                route: "/prompt".into(),
                body,
                result: Ok(json!({ "run_id": 1 })),
            })
            .unwrap();
        app.tick();
        assert!(app.pending.is_none());
        assert_eq!(app.editor.text(), "edited while sending");
    }
    #[tokio::test]
    async fn withdrawing_preserves_existing_draft_and_attachment_bytes() {
        let mut app = app();
        app.editor.restore("current", 7).unwrap();
        app.restore_blocks(vec![
            eden_protocol::coding::Block::Text {
                text: "queued".into(),
            },
            eden_protocol::coding::Block::File {
                name: "old.pdf".into(),
                media_type: "application/pdf".into(),
                data: "c25hcHNob3Q=".into(),
            },
        ]);
        assert_eq!(app.editor.text(), "current\nqueued");
        assert_eq!(app.attachments[0].bytes.as_ref(), b"snapshot");
    }
    #[tokio::test]
    async fn inspector_reuses_layout_and_survives_resize() {
        let mut app = app();
        app.messages.push(Message::new(
            1,
            Role::Tool,
            "read file",
            (0..1000).map(|i| format!("line {i}\n")).collect::<String>(),
        ));
        app.preferences.inspector = true;
        crate::text::INSPECTOR_LAYOUTS.with(|n| n.set(0));
        for _ in 0..8 {
            let r = Rect::new(0, 0, 120, 36);
            crate::view::render(&mut app, &mut Buffer::empty(r), r, false);
        }
        assert_eq!(crate::text::INSPECTOR_LAYOUTS.with(|n| n.get()), 1);
        for (w, h) in [(40, 12), (80, 24), (160, 50), (1, 1), (0, 0)] {
            let r = Rect::new(0, 0, w, h);
            crate::view::render(&mut app, &mut Buffer::empty(r), r, false);
        }
    }
}

fn read_attachment(path: &Path) -> std::result::Result<Attachment, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let name = path
        .file_name()
        .ok_or("attachment needs a filename")?
        .to_string_lossy()
        .into_owned();
    let image = matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "tif" | "tiff" | "bmp"
    );
    Ok(Attachment {
        name,
        source: path.to_string_lossy().into_owned(),
        bytes: bytes.into(),
        image,
        media_type: None,
    })
}

#[cfg(test)]
mod repair_tests {
    use super::tests::app;
    use super::*;
    use eden_protocol::coding::Block;
    use ratatui::{buffer::Buffer, layout::Rect};
    fn frame(app: &mut App) -> String {
        let rect = Rect::new(0, 0, 120, 32);
        let mut buffer = Buffer::empty(rect);
        crate::view::render(app, &mut buffer, rect, false);
        buffer
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
    }
    #[tokio::test]
    async fn completion_undo_removes_snapshot_and_redo_keeps_original_bytes() {
        let mut app = app();
        app.editor.restore("@fi", 3).unwrap();
        app.changed();
        let replacement = crate::autocomplete::Replacement {
            token: crate::autocomplete::token("@fi", 3).unwrap(),
            original: "@fi".into(),
        };
        app.tx
            .send(Update::Completion {
                replacement,
                insert: "@file.txt ".into(),
                source: "@fi".into(),
                cursor: 3,
                generation: 0,
                result: Ok(Attachment {
                    name: "file.txt".into(),
                    source: "removed-source".into(),
                    bytes: b"original".to_vec().into(),
                    image: false,
                    media_type: None,
                }),
            })
            .unwrap();
        app.tick();
        assert_eq!(app.attachments.len(), 1);
        app.key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(app.editor.text(), "@fi");
        assert_eq!(app.content().unwrap().len(), 1);
        app.key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::ALT));
        assert_eq!(app.attachments[0].bytes.as_ref(), b"original");
        assert!(
            app.content()
                .unwrap()
                .iter()
                .any(|b| matches!(b,Block::Text{text} if text.contains("original")))
        );
    }
    #[tokio::test]
    async fn history_round_trip_restores_the_current_draft() {
        let mut app = app();
        app.input_history = vec!["earlier request".into()];
        app.editor.restore("unfinished", 0).unwrap();
        app.changed();
        let draft = app.draft();
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "earlier request");
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.draft(), draft);
    }
    #[tokio::test]
    async fn full_artifact_scrolls_and_does_not_replace_another_tool() {
        let mut app = app();
        app.messages = vec![
            Message::new(1, Role::Tool, "First", "first output"),
            Message::new(2, Role::Tool, "Second", "second output"),
        ];
        app.tx
            .send(Update::Artifact {
                record: 1,
                result: Ok(format!(
                    "{}\nLAST_ARTIFACT_LINE",
                    (0..500)
                        .map(|i| format!("artifact line {i}\n"))
                        .collect::<String>()
                )),
            })
            .unwrap();
        app.tick();
        app.inspector_scroll = usize::MAX;
        assert!(frame(&mut app).contains("LAST_ARTIFACT_LINE"));
        assert!(app.inspector_scroll > 0);
        app.selected = 1;
        let rendered = frame(&mut app);
        assert!(rendered.contains("second output"));
        assert!(!rendered.contains("LAST_ARTIFACT_LINE"));
    }
    #[tokio::test]
    async fn authoritative_rejection_releases_admission_and_preserves_draft() {
        let mut app = app();
        app.editor.restore("next", 4).unwrap();
        let body = json!({ "request_id": "one" });
        app.pending = Some(("/prompt".into(), body.clone(), app.draft()));
        app.tx
            .send(Update::Reply {
                route: "/prompt".into(),
                body,
                result: Err(Fault::new("Unavailable", "session", "shell is active")),
            })
            .unwrap();
        app.tick();
        assert!(app.pending.is_none());
        assert!(app.uncertain.is_none());
        assert_eq!(app.editor.text(), "next");
    }
    #[tokio::test]
    async fn unknown_receipt_has_an_explicit_keep_draft_exit() {
        let mut app = app();
        app.editor.restore("unsent", 6).unwrap();
        app.uncertain = Some((
            "/prompt".into(),
            json!({ "request_id": "lost" }),
            app.draft(),
        ));
        app.tx
            .send(Update::Reply {
                route: "/request-status".into(),
                body: json!({ "request_id": "lost" }),
                result: Ok(json!({ "status": "unknown" })),
            })
            .unwrap();
        app.tick();
        assert!(matches!(&app.dialog,Some(Dialog::Palette{kind,..}) if kind=="resolve-request"));
        app.choose("resolve-request", "keep");
        assert!(app.uncertain.is_none());
        assert_eq!(app.editor.text(), "unsent");
    }
    #[tokio::test]
    async fn encoded_clipboard_image_keeps_its_mime_at_submission() {
        let mut app = app();
        app.tx
            .send(Update::Clipboard(
                app.clipboard_request().unwrap(),
                Ok(crate::clipboard::ClipboardContent::Image {
                    media_type: "image/tiff".into(),
                    bytes: vec![0, 255, 1],
                }),
            ))
            .unwrap();
        app.tick();
        assert!(
            matches!(&app.content().unwrap()[1],Block::Image{media_type,..} if media_type=="image/tiff")
        );
    }
}

#[cfg(test)]
mod undo_alias_tests {
    use super::*;
    #[tokio::test]
    async fn all_native_undo_aliases_keep_attachment_state_in_the_same_transaction() {
        for (code, kind) in [
            (KeyCode::Char('-'), KeyEventKind::Press),
            (KeyCode::Char('z'), KeyEventKind::Repeat),
            (KeyCode::Char('-'), KeyEventKind::Repeat),
        ] {
            let mut app = super::tests::app();
            app.editor.restore("@fi", 3).unwrap();
            app.changed();
            app.tx
                .send(Update::Completion {
                    replacement: crate::autocomplete::Replacement {
                        token: crate::autocomplete::token("@fi", 3).unwrap(),
                        original: "@fi".into(),
                    },
                    insert: "@file ".into(),
                    source: "@fi".into(),
                    cursor: 3,
                    generation: 0,
                    result: Ok(Attachment {
                        name: "file".into(),
                        source: "missing".into(),
                        bytes: b"snapshot".to_vec().into(),
                        image: false,
                        media_type: None,
                    }),
                })
                .unwrap();
            app.tick();
            let mut key = KeyEvent::new(code, KeyModifiers::CONTROL);
            key.kind = kind;
            app.key(key);
            assert_eq!(app.editor.text(), "@fi", "{key:?}");
            assert!(app.attachments.is_empty(), "{key:?}");
        }
    }
}

#[cfg(test)]
mod form_reopen_tests {
    use super::*;
    fn view() -> eden_protocol::presentation::LiveView {
        serde_json::from_value(json!({
            "owner": "custom",
            "run_id": 1,
            "revision": 1,
            "active": true,
            "id": "settings",
            "slot": "panel",
            "title": "Settings",
            "fallback": "Settings",
            "source": null,
            "platforms": [],
            "nodes": [{
                "kind": "configuration_form",
                "id": "fields",
                "binding": { "instance": "custom", "generation": 1, "revision": 1, "profile": 0 },
                "fields": [{
                    "path": "/endpoint",
                    "label": "Endpoint",
                    "description": null,
                    "control": "text",
                    "value": "old",
                    "options": [],
                    "source": "composition",
                    "writable": true,
                    "configured": true,
                }],
            }],
        }))
        .unwrap()
    }
    fn advance(app: &mut App, revision: u64) {
        let live = &mut app.snapshot.presentation.views[0];
        live.revision = revision;
        if let eden_protocol::presentation::Node::ConfigurationForm {
            binding, fields, ..
        } = &mut live.view.nodes[0]
        {
            binding.revision = revision;
            binding.generation = Some(revision);
            fields[0].value = Some(json!(format!("server-{revision}")));
        }
    }

    #[tokio::test]
    async fn refresh_uses_latest_target_without_submitting_or_validating_drafts() {
        let mut app = super::tests::app();
        app.snapshot.presentation.views.push(view());
        app.open_live("custom|settings|fields");
        if let Some(Dialog::Form { fields, .. }) = &mut app.dialog {
            fields[0].value = "unfinished ordinary draft".into();
            fields[0].kind = "json".into();
            let mut secret = Field::text("Secret", "");
            secret.private = true;
            secret.value = "PRIVATE_REFRESH_CANARY".into();
            fields.push(secret);
        }
        advance(&mut app, 2);
        app.submit_form("apply");
        assert!(
            app.form_retry.is_none(),
            "Apply must retain the old binding CAS"
        );
        app.submit_form("refresh");
        let pending = app
            .form_retry
            .as_ref()
            .expect("Refresh must accept unfinished drafts");
        assert_eq!(pending.0, "/action");
        assert_eq!(pending.1["revision"], 2);
        assert_eq!(pending.1["values"]["binding"]["revision"], 2);
        assert_eq!(pending.1["values"]["binding"]["generation"], 2);
        assert_eq!(pending.1["values"]["edits"], json!([]));
        assert!(!pending.1.to_string().contains("PRIVATE_REFRESH_CANARY"));
    }

    #[tokio::test]
    async fn refresh_receipt_rebinds_its_draft_in_either_snapshot_order() {
        for snapshot_first in [false, true] {
            let mut app = super::tests::app();
            app.snapshot.presentation.views.push(view());
            app.open_live("custom|settings|fields");
            if let Some(Dialog::Form { fields, .. }) = &mut app.dialog {
                fields[0].value = "my draft".into();
            }
            advance(&mut app, 2);
            app.submit_form("refresh");
            let body = app.form_retry.as_ref().unwrap().1.clone();
            assert_eq!(body["values"]["binding"]["revision"], 2);
            let mut unrelated = body.clone();
            unrelated["request_id"] = json!("other-refresh");
            app.tx
                .send(Update::Reply {
                    route: "/action".into(),
                    body: unrelated,
                    result: Ok(json!([{ "value": 3 }])),
                })
                .unwrap();
            app.tick();
            assert!(app.form_retry.is_some());
            assert!(app.form_refresh.is_none());
            if snapshot_first {
                advance(&mut app, 3);
                app.rebind_applied_form();
            }
            app.tx
                .send(Update::Reply {
                    route: "/action".into(),
                    body: body.clone(),
                    result: Ok(json!([{ "value": 3 }])),
                })
                .unwrap();
            app.tick();
            assert!(app.form_retry.is_none());
            // An explicitly recovered request may produce a second receipt before
            // the follower delivers the refreshed snapshot.
            app.tx
                .send(Update::Reply {
                    route: "/action".into(),
                    body,
                    result: Ok(json!([{ "value": 3 }])),
                })
                .unwrap();
            app.tick();
            assert!(app.form_refresh.is_some());
            if !snapshot_first {
                app.rebind_applied_form();
                assert_eq!(
                    app.form_target
                        .as_ref()
                        .unwrap()
                        .binding
                        .as_ref()
                        .unwrap()
                        .revision,
                    1,
                    "the pre-refresh snapshot must not satisfy the refresh receipt"
                );
                advance(&mut app, 3);
            }
            app.rebind_applied_form();
            assert_eq!(
                app.form_target
                    .as_ref()
                    .unwrap()
                    .binding
                    .as_ref()
                    .unwrap()
                    .revision,
                3
            );
            assert!(
                matches!(&app.dialog, Some(Dialog::Form { fields, status, .. })
                if fields[0].value == "my draft" && fields[0].initial == "server-3"
                    && status.starts_with("Refreshed"))
            );
            app.submit_form("apply");
            let applied = &app.form_retry.as_ref().unwrap().1;
            assert_eq!(applied["values"]["binding"]["revision"], 3);
            assert_eq!(applied["values"]["edits"][0]["value"], "my draft");
        }
    }

    #[tokio::test]
    async fn reopening_a_clean_or_already_applied_form_uses_the_latest_binding() {
        for submitted in [false, true] {
            let mut app = super::tests::app();
            app.snapshot.presentation.views.push(view());
            app.open_live("custom|settings|fields");
            if submitted && let Some(Dialog::Form { fields, .. }) = &mut app.dialog {
                fields[0].value = "current".into();
            }
            app.dialog_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            let live = &mut app.snapshot.presentation.views[0];
            live.revision = 2;
            if let eden_protocol::presentation::Node::ConfigurationForm {
                binding, fields, ..
            } = &mut live.view.nodes[0]
            {
                binding.revision = 2;
                binding.generation = Some(2);
                fields[0].value = Some(json!("current"));
                fields[0].label = "Destination (author label)".into();
            }
            app.open_live("custom|settings|fields");
            assert_eq!(
                app.form_target
                    .as_ref()
                    .unwrap()
                    .binding
                    .as_ref()
                    .unwrap()
                    .revision,
                2,
                "submitted={submitted}"
            );
            assert!(
                matches!(&app.dialog,Some(Dialog::Form{fields,..}) if fields[0].value=="current" && fields[0].label=="Destination (author label)")
            );
        }
    }
    #[tokio::test]
    async fn reopening_a_changed_binding_retains_an_unapplied_draft() {
        let mut app = super::tests::app();
        app.snapshot.presentation.views.push(view());
        app.open_live("custom|settings|fields");
        if let Some(Dialog::Form { fields, .. }) = &mut app.dialog {
            fields[0].value = "local draft".into();
        }
        app.dialog_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        if let eden_protocol::presentation::Node::ConfigurationForm {
            binding, fields, ..
        } = &mut app.snapshot.presentation.views[0].view.nodes[0]
        {
            binding.revision = 2;
            fields[0].value = Some(json!("remote value"));
        }
        app.open_live("custom|settings|fields");
        assert_eq!(
            app.form_target
                .as_ref()
                .unwrap()
                .binding
                .as_ref()
                .unwrap()
                .revision,
            1
        );
        assert!(
            matches!(&app.dialog,Some(Dialog::Form{fields,..}) if fields[0].value=="local draft")
        );
    }
    #[tokio::test]
    async fn acknowledged_clear_and_inherit_do_not_reopen_as_conflicting_drafts() {
        for operation in ["clear", "inherit"] {
            for close_before_reply in [false, true] {
                let mut app = super::tests::app();
                app.snapshot.presentation.views.push(view());
                app.open_live("custom|settings|fields");
                if let Some(Dialog::Form { fields, .. }) = &mut app.dialog {
                    fields[0].clear = operation == "clear";
                    fields[0].inherit = operation == "inherit";
                }
                let body = json!({
                    "owner": "custom",
                    "view_id": "settings",
                    "action": "fields:apply",
                    "values": {
                        "binding": {
                            "instance": "custom",
                            "generation": 1,
                            "revision": 1,
                            "profile": 0,
                        },
                        "edits": [{ "operation": operation, "path": "/endpoint" }],
                    },
                });
                if close_before_reply {
                    app.dialog_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                }
                app.tx
                    .send(Update::Reply {
                        route: "/action".into(),
                        body,
                        result: Ok(json!({ "status": "applied" })),
                    })
                    .unwrap();
                app.tick();
                if !close_before_reply {
                    app.dialog_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                }
                if let eden_protocol::presentation::Node::ConfigurationForm { binding, .. } =
                    &mut app.snapshot.presentation.views[0].view.nodes[0]
                {
                    binding.revision = 2;
                    binding.generation = Some(2);
                }
                app.open_live("custom|settings|fields");
                assert_eq!(
                    app.form_target
                        .as_ref()
                        .unwrap()
                        .binding
                        .as_ref()
                        .unwrap()
                        .revision,
                    2,
                    "{operation}, close_before_reply={close_before_reply}"
                );
            }
        }
    }
}

fn edit_matches(field: &Field, edit: &Value) -> bool {
    match edit["operation"].as_str() {
        Some("set") => {
            !field.clear
                && !field.inherit
                && field.parsed_value().ok().as_ref() == Some(&edit["value"])
        }
        Some("clear") => field.clear,
        Some("inherit") => field.inherit,
        _ => false,
    }
}
fn acknowledge_edits(fields: &mut [Field], request: &Value) {
    for edit in request["values"]["edits"].as_array().into_iter().flatten() {
        if let Some(field) = fields
            .iter_mut()
            .find(|f| f.key == edit["path"].as_str().unwrap_or(""))
            && !field.private
            && edit_matches(field, edit)
        {
            field.initial = field.value.clone();
            field.clear = false;
            field.inherit = false;
        }
    }
}

#[cfg(test)]
mod clipboard_focus_tests {
    use super::*;
    #[tokio::test]
    async fn late_private_field_paste_never_becomes_a_saved_composer_draft() {
        let mut app = super::tests::app();
        let mut field = Field::text("Private", "");
        field.private = true;
        field.key = "/token".into();
        app.dialog = Some(Dialog::Form {
            title: "Private".into(),
            fields: vec![field],
            selected: 0,
            status: String::new(),
        });
        app.form_target = Some(crate::forms::Target {
            owner: "config".into(),
            view: "settings".into(),
            revision: 1,
            node: "fields".into(),
            action: "apply".into(),
            binding: None,
        });
        app.tx
            .send(Update::Clipboard(
                app.clipboard_request().unwrap(),
                Ok(crate::clipboard::ClipboardContent::Text(
                    "PRIVATE_CLIPBOARD_CANARY".into(),
                )),
            ))
            .unwrap();
        app.dialog_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.tick();
        assert!(app.draft().text.is_empty());
        assert!(
            !serde_json::to_string(&app.draft())
                .unwrap()
                .contains("PRIVATE_CLIPBOARD_CANARY")
        );
    }
}

#[cfg(test)]
mod reference_draft_tests {
    use super::*;
    #[tokio::test]
    async fn reference_budget_rejection_and_unknown_receipt_keep_fixed_draft() {
        for error in [
            Fault::new(
                "ReferenceBudgetExceeded",
                "session-reference",
                "reduce selection",
            ),
            Fault::new("InputFailure", "live-client", "lost receipt"),
        ] {
            let mut app = super::tests::app();
            app.references
                .push(crate::references::tests::frozen().into());
            app.editor.restore("question", 0).unwrap();
            app.changed();
            app.submit();
            let (route, body, draft) = app.pending.clone().unwrap();
            assert_eq!(body["references"][0]["source"]["head"], 4);
            app.tx
                .send(Update::Reply {
                    route,
                    body,
                    result: Err(error),
                })
                .unwrap();
            app.tick();
            assert_eq!(app.references, draft.references);
            assert_eq!(app.editor.text(), "question");
        }
    }
    #[tokio::test]
    async fn withdrawal_restores_fixed_references_and_undo_redo_keeps_them_owned() {
        let mut app = super::tests::app();
        app.editor.restore("current", 0).unwrap();
        app.changed();
        let reference = crate::references::tests::frozen();
        app.tx
            .send(Update::Reply {
                route: "/queue/withdraw".into(),
                body: json!({ "id": 1 }),
                result: Ok(json!([{
                    "id": 1,
                    "kind": "follow_up",
                    "content": [{ "type": "text", "text": "queued" }],
                    "references": [reference],
                }])),
            })
            .unwrap();
        app.tick();
        assert_eq!(app.editor.text(), "current\nqueued");
        assert_eq!(*app.references[0], reference);
        app.undo_draft(false);
        assert!(app.references.is_empty());
        app.undo_draft(true);
        assert_eq!(*app.references[0], reference);
    }
    #[tokio::test]
    async fn admission_clears_only_the_matching_reference_draft() {
        for edit_while_pending in [false, true] {
            let mut app = super::tests::app();
            app.references
                .push(crate::references::tests::frozen().into());
            app.changed();
            app.submit();
            let (route, body, _) = app.pending.clone().unwrap();
            if edit_while_pending {
                std::sync::Arc::make_mut(&mut app.references[0]).id = "new draft identity".into();
                app.changed();
            }
            app.tx
                .send(Update::Reply {
                    route,
                    body,
                    result: Ok(json!({ "run_id": 42 })),
                })
                .unwrap();
            app.tick();
            assert_eq!(app.references.is_empty(), !edit_while_pending);
        }
    }
}
