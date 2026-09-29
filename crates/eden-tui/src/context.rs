//! Context drafts are independent of the composer and change only through explicit actions.
mod images;
use crate::{
    app::{App, send_editor_key},
    view::{Geometry, Palette},
};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use eden_protocol::{
    Fault,
    coding::{Block, Item},
    context_edit::{Apply, Document, Edit, Entry, Scope, Snapshot},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Modifier,
    widgets::{Paragraph, Widget, Wrap},
};
use serde_json::{Value, json};

#[derive(Default)]
pub struct Context {
    pub open: bool,
    snapshot: Option<Snapshot>,
    draft: Option<Document>,
    selected: usize,
    scope: Scope,
    view: usize,
    scroll: usize,
    pub editing: Option<Editing>,
    status: String,
    preview: Option<String>,
    pending: Option<Value>,
    waiting: bool,
    inspect: Option<(String, bool)>,
    confirm_reload: bool,
    display_cache: Option<(usize, usize, std::sync::Arc<str>)>,
    pub usage: String,
    images: images::Images,
    policies: Vec<eden_protocol::context_edit::Policy>,
    management: Option<ManagementDraft>,
    operation: Option<Operation>,
    operation_result: String,
    refreshed: Option<Snapshot>,
    refresh_after_operation: bool,
    open_management_after_inspect: Option<bool>,
}
#[derive(Clone, Copy)]
pub enum Editing {
    Text(usize),
    Item(usize),
    Tools,
    Document,
    Branch,
    Instructions,
}
struct ManagementDraft {
    rebuild: bool,
    branch: String,
    instructions: String,
    edits: Vec<(u64, String, bool)>,
    selected: usize,
}
struct Operation {
    route: &'static str,
    body: Value,
    run: Option<u64>,
}

impl Context {
    fn display(&mut self) -> std::sync::Arc<str> {
        if let Some((view, selected, text)) = &self.display_cache
            && *view == self.view
            && *selected == self.selected
        {
            return text.clone();
        }
        let text = match (&self.snapshot, &self.draft, self.view) {
            (Some(snapshot), _, 1) => pretty(&snapshot.original),
            (_, _, 2) => self
                .preview
                .clone()
                .unwrap_or_else(|| "Press p to validate and preview the current draft".into()),
            (Some(snapshot), _, 3) => snapshot
                .last_request
                .as_ref()
                .map(pretty)
                .unwrap_or_else(|| "No captured actual request".into()),
            (_, _, 4) => {
                let mut lines = vec![
                    "Policies run in the configured order at their named boundary. Inspector \
                     reads run no policies."
                        .to_owned(),
                    "Use /config coding to enable, configure or reorder policies.".into(),
                ];
                for (index, policy) in self.policies.iter().enumerate() {
                    lines.push(format!(
                        "\n{}. {} · {} · {:?}\nRole: {}\n{}",
                        index + 1,
                        policy.name,
                        if policy.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        policy.boundary,
                        policy.role,
                        pretty(&policy.config)
                    ));
                }
                if self.policies.is_empty() {
                    lines.push("\nNo configured context policies.".into());
                }
                lines.join("\n")
            }
            (_, _, 5) => format!(
                "{}\n\n{}",
                self.operation_result,
                self.refreshed
                    .as_ref()
                    .map(|s| format!(
                        "Refreshed result · branch {} · revision {}\n{}",
                        s.revision.branch,
                        s.revision.sequence,
                        pretty(&s.effective)
                    ))
                    .unwrap_or_default()
            ),
            (Some(snapshot), _, 6) => {
                let snapshot = self.refreshed.as_ref().unwrap_or(snapshot);
                format!(
                    "Model and effective budget (unknown values remain unknown)\n{}\n\nCompaction \
                     budget and per-field source\n{}\n\nImage limits\n{}\n\ng opens /config \
                     coding for model overrides. Existing image versions are preserved unless \
                     explicitly omitted or re-adapted.",
                    pretty(&snapshot.model),
                    pretty(&snapshot.budget),
                    pretty(&snapshot.image_limits)
                )
            }
            (_, Some(draft), _) => draft
                .entries
                .get(self.selected)
                .map(|e| pretty(&e.item))
                .unwrap_or_else(|| pretty(&draft.tools)),
            _ => String::new(),
        };
        let text: std::sync::Arc<str> = text.into();
        self.display_cache = Some((self.view, self.selected, text.clone()));
        text
    }
    pub fn update_usage(&mut self, records: &[eden_protocol::coding::Record]) {
        if self.images.history(records) && self.view == 7 {
            self.images.load();
        }
        let used: std::collections::BTreeSet<_> = records
            .iter()
            .filter(|r| r.kind == "model_request")
            .flat_map(|r| {
                r.payload["context_edits"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_u64)
            })
            .collect();
        self.usage = records
            .iter()
            .filter(|r| r.kind == "context_edit")
            .map(|r| {
                format!(
                    "#{} {}",
                    r.sequence,
                    if used.contains(&r.sequence) {
                        "used"
                    } else {
                        "pending"
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(" · ");
    }
    fn invalidate(&mut self) {
        self.preview = None;
        self.display_cache = None;
        self.view = 0;
        self.scroll = 0;
        self.status = "Draft changed · preview before applying".into();
    }
    fn rebase(&mut self, latest: Snapshot) -> Result<(), String> {
        let old = self.snapshot.as_ref().ok_or("No captured base")?;
        let draft = self.draft.as_ref().ok_or("No draft")?;
        let unchanged = draft
            .entries
            .iter()
            .filter(|entry| {
                old.effective.entries.iter().any(|e| {
                    e.id == entry.id
                        && serde_json::to_value(e).ok() == serde_json::to_value(entry).ok()
                })
            })
            .map(|e| e.id.clone())
            .collect();
        let edit = Edit {
            base: old.effective.entries.iter().map(|e| e.id.clone()).collect(),
            replacement: draft.entries.clone(),
            unchanged,
            tools: (serde_json::to_value(&draft.tools).ok()
                != serde_json::to_value(&old.effective.tools).ok())
            .then(|| draft.tools.clone()),
            scope: self.scope,
            source: "tui".into(),
        };
        // Let the same structural rules reject missing edited identities and broken tool groups.
        let rebased = latest.effective.apply(&edit).map_err(|e| e.to_string())?;
        self.images.capture(&latest);
        self.policies = latest.policies.clone();
        self.snapshot = Some(latest);
        self.draft = Some(rebased);
        self.invalidate();
        self.status = "Rebased onto latest input · review and preview again".into();
        Ok(())
    }
    fn capture(&mut self, snapshot: Snapshot) {
        self.images.reset_choices();
        self.images.capture(&snapshot);
        self.policies = snapshot.policies.clone();
        self.draft = Some(snapshot.effective.clone());
        self.snapshot = Some(snapshot);
        self.selected = 0;
        self.preview = None;
        self.display_cache = None;
        self.view = 0;
        self.scroll = 0;
    }
    fn preview(&mut self) -> Result<(), String> {
        let draft = self.draft.as_ref().ok_or("Wait for inspection")?;
        draft.validate().map_err(|e| e.to_string())?;
        let original = &self.snapshot.as_ref().ok_or("Missing base")?.original;
        let mut lines = vec!["Original → draft (unchanged entries omitted)".to_owned()];
        for entry in &original.entries {
            if !draft.entries.iter().any(|e| {
                e.id == entry.id && serde_json::to_value(e).ok() == serde_json::to_value(entry).ok()
            }) {
                lines.push(format!("- {} {}", entry.id, pretty(&entry.item)));
            }
        }
        for (index, entry) in draft.entries.iter().enumerate() {
            let original_index = original.entries.iter().position(|e| e.id == entry.id);
            if original_index.is_none_or(|i| {
                serde_json::to_value(&original.entries[i]).ok() != serde_json::to_value(entry).ok()
            }) {
                lines.push(format!("+ {} {}", entry.id, pretty(&entry.item)));
            } else if original_index != Some(index) {
                lines.push(format!(
                    "↕ {} position {} → {}",
                    entry.id,
                    original_index.unwrap_or(0) + 1,
                    index + 1
                ));
            }
        }
        if serde_json::to_value(&original.tools).ok() != serde_json::to_value(&draft.tools).ok() {
            lines.push(format!(
                "- tools {}\n+ tools {}",
                pretty(&original.tools),
                pretty(&draft.tools)
            ));
        }
        if lines.len() == 1 {
            lines.push("No difference from original".into());
        }
        self.preview = Some(lines.join("\n"));
        self.display_cache = None;
        self.view = 2;
        self.scroll = 0;
        self.status =
            "Structure valid · a applies this preview at the next safe request boundary".into();
        Ok(())
    }
}
fn pretty(value: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

impl App {
    pub(crate) fn open_context(&mut self) {
        self.context.open = true;
        self.autocomplete.dismiss();
        self.clipboard_generation += 1;
        if self.context.snapshot.is_none() && self.context.inspect.is_none() {
            self.inspect_context(false);
        }
    }
    pub(crate) fn poll_context_images(&mut self) -> bool {
        self.context.images.poll()
    }
    fn inspect_context(&mut self, rebase: bool) {
        if self.context.images.pending.is_some()
            || self.context.pending.is_some()
            || self.context.operation.is_some()
            || self.context.inspect.is_some()
        {
            self.context.status = "Finish the pending operation first; draft retained".into();
            return;
        }
        let id = self.id();
        self.context.inspect = Some((id.clone(), rebase));
        self.context.status = "Inspecting shared model input…".into();
        self.dispatch("/context/inspect", json!({ "request_id": id }));
    }
    pub(crate) fn context_reply(
        &mut self,
        route: &str,
        body: &Value,
        result: Result<Value, Fault>,
    ) {
        if route == "/context/images" {
            self.context_images_reply(body, result);
            return;
        }
        if matches!(route, "/context/rebuild" | "/context/compact" | "/terminal") {
            self.context_operation_reply(route, body, result);
            return;
        }
        if route == "/context/inspect" {
            let Some((id, rebase)) = self.context.inspect.as_ref() else {
                return;
            };
            if body["request_id"] != *id {
                return;
            }
            let rebase = *rebase;
            self.context.inspect = None;
            match result.and_then(|v| {
                self.context.display_cache = None;
                serde_json::from_value::<Snapshot>(v)
                    .map_err(|e| Fault::new("InvalidContext", "tui", e.to_string()))
            }) {
                Ok(snapshot) if self.context.refresh_after_operation => {
                    self.context.refresh_after_operation = false;
                    self.context.policies = snapshot.policies.clone();
                    self.context.refreshed = Some(snapshot);
                    self.context.view = 5;
                    self.context.status = "Operation completed · result refreshed · original \
                                           draft retained; r reloads or b rebases"
                        .into();
                }
                Ok(snapshot) if rebase => {
                    if let Err(error) = self.context.rebase(snapshot) {
                        self.context.status = format!("Rebase failed · draft retained: {error}");
                    }
                }
                Ok(snapshot) => {
                    self.context.capture(snapshot);
                    self.context.status =
                        "Captured shared model input · edits affect future requests".into();
                }
                Err(error) => {
                    self.context.refresh_after_operation = false;
                    self.context.status = format!("Inspection failed · draft retained: {error}")
                }
            }
            if let Some(rebuild) = self.context.open_management_after_inspect.take()
                && self.context.snapshot.is_some()
            {
                self.open_context_management(rebuild);
            }
        } else if route == "/context/apply" {
            if !self
                .context
                .pending
                .as_ref()
                .is_some_and(|p| p["request_id"] == body["request_id"])
            {
                return;
            }
            self.context.waiting = false;
            match result {
                Ok(value) => match serde_json::from_value::<Snapshot>(value) {
                    Ok(snapshot) => {
                        self.context.pending = None;
                        self.context.capture(snapshot);
                        self.context.status = "Applied · queued for a safe request boundary; \
                                               usage appears in request records"
                            .into();
                    }
                    Err(error) => {
                        self.context.status =
                            format!("Receipt unreadable · t retries the same transaction: {error}")
                    }
                },
                Err(error) => {
                    if error.source != "live-client" {
                        self.context.pending = None;
                    }
                    self.context.status = format!(
                        "Apply failed · draft retained · b rebase / r reload / t retry: {error}"
                    );
                }
            }
        }
    }
    fn context_edit(&mut self, editing: Editing) {
        let Some(draft) = &self.context.draft else {
            return;
        };
        let value = match editing {
            Editing::Text(index) => match &draft.entries[index].item {
                Item::Message { content, .. } => content
                    .iter()
                    .find_map(|b| match b {
                        Block::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default(),
                _ => {
                    self.context.status =
                        "Use j for structural JSON on tool calls/results or provider state".into();
                    return;
                }
            },
            Editing::Item(index) => pretty(&draft.entries[index].item),
            Editing::Tools => pretty(&draft.tools),
            Editing::Document => pretty(draft),
            Editing::Branch => self
                .context
                .management
                .as_ref()
                .map(|m| m.branch.clone())
                .unwrap_or_default(),
            Editing::Instructions => self
                .context
                .management
                .as_ref()
                .map(|m| m.instructions.clone())
                .unwrap_or_default(),
        };
        if self.field_editor.restore(&value, 0).is_ok() {
            self.context.editing = Some(editing);
            self.context.status =
                "Editing local draft · Ctrl+S saves locally · Esc cancels this field".into();
        }
    }
    fn save_context_field(&mut self) -> Result<(), String> {
        let text = self.field_editor.text();
        if matches!(
            self.context.editing,
            Some(Editing::Branch | Editing::Instructions)
        ) {
            if let Some(management) = &mut self.context.management {
                if matches!(self.context.editing, Some(Editing::Branch)) {
                    management.branch = text;
                } else {
                    management.instructions = text;
                }
            }
            self.context.editing = None;
            self.context.status = "Operation options saved locally · choose Start to run".into();
            return Ok(());
        }
        let Some(draft) = self.context.draft.as_mut() else {
            return Ok(());
        };
        match self.context.editing {
            Some(Editing::Text(index)) => {
                if let Item::Message { content, .. } = &mut draft.entries[index].item {
                    if let Some(Block::Text { text: current }) =
                        content.iter_mut().find(|b| matches!(b, Block::Text { .. }))
                    {
                        *current = text;
                    } else {
                        content.insert(0, Block::Text { text });
                    }
                }
            }
            Some(Editing::Item(index)) => {
                draft.entries[index].item =
                    serde_json::from_str(&text).map_err(|e| e.to_string())?
            }
            Some(Editing::Tools) => {
                draft.tools = serde_json::from_str(&text).map_err(|e| e.to_string())?
            }
            Some(Editing::Document) => {
                *draft = serde_json::from_str(&text).map_err(|e| e.to_string())?
            }
            Some(Editing::Branch | Editing::Instructions) | None => return Ok(()),
        }
        self.context.selected = self.context.selected.min(draft.entries.len());
        self.context.editing = None;
        self.context.invalidate();
        Ok(())
    }
    pub(crate) fn context_key(&mut self, key: KeyEvent) {
        if self.context.editing.is_some() {
            if key.code == KeyCode::Esc {
                self.context.editing = None;
                self.context.status = "Field cancelled · draft retained".into();
            } else if key.code == KeyCode::Char('s')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                if let Err(error) = self.save_context_field() {
                    self.context.status = format!("Invalid JSON · editor retained: {error}");
                }
            } else {
                send_editor_key(&mut self.field_editor, key);
            }
            return;
        }
        if key.kind == KeyEventKind::Repeat
            && !matches!(
                key.code,
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
            )
        {
            return;
        }
        if key.code == KeyCode::Esc {
            if self.context.management.is_some() {
                self.context.management = None;
                self.context.status = "Operation form closed · context draft retained".into();
                return;
            }
            self.context.open = false;
            return;
        }
        if self.context.confirm_reload {
            self.context.confirm_reload = false;
            if key.code == KeyCode::Char('y') {
                self.inspect_context(false);
            } else {
                self.context.status = "Reload cancelled · draft retained".into();
            }
            return;
        }
        if key.code == KeyCode::Char('t') {
            if !self.context.waiting
                && let Some(body) = self.context.images.pending.clone()
            {
                self.context.waiting = true;
                self.dispatch("/context/images", body);
                return;
            }
            if !self.context.waiting && self.context.operation.is_some() {
                self.retry_context_operation();
                return;
            }
            if !self.context.waiting
                && let Some(body) = self.context.pending.clone()
            {
                self.context.waiting = true;
                self.dispatch("/context/apply", body);
            }
            return;
        }
        if self.context.images.pending.is_some()
            || self.context.pending.is_some()
            || self.context.operation.is_some()
            || self.context.inspect.is_some()
        {
            return;
        }
        if self.context.management.is_some() {
            self.context_management_key(key);
            return;
        }
        if self.context.view == 7
            && !matches!(
                key.code,
                KeyCode::Tab | KeyCode::Char('b' | 'r' | 'B' | 'P')
            )
        {
            self.context_images_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('B') => {
                self.context.view = 6;
                self.context.scroll = 0;
            }
            KeyCode::Char('I') => {
                self.context.view = 7;
                self.context.images.load();
            }
            KeyCode::Char('R') => self.open_context_management(true),
            KeyCode::Char('c') => self.open_context_management(false),
            KeyCode::Char('P') => {
                self.context.view = 4;
                self.context.scroll = 0;
            }
            KeyCode::Char('g') if matches!(self.context.view, 4 | 6) => {
                self.context.open = false;
                self.dispatch("/configuration/open", json!({ "instance": "coding" }));
            }
            KeyCode::Char('r') => {
                self.context.confirm_reload = true;
                self.context.status = "Discard the context draft and reload latest? y confirms; \
                                       any other key keeps draft"
                    .into();
            }
            KeyCode::Char('b') => self.inspect_context(true),
            KeyCode::Tab => {
                self.context.view = (self.context.view + 1) % 8;
                if self.context.view == 7 {
                    self.context.images.load();
                };
                self.context.scroll = 0;
            }
            KeyCode::PageDown => self.context.scroll = self.context.scroll.saturating_add(10),
            KeyCode::PageUp => self.context.scroll = self.context.scroll.saturating_sub(10),
            KeyCode::Char('p') => {
                if let Err(error) = self.context.preview() {
                    self.context.status = format!("Preview rejected · draft retained: {error}");
                }
            }
            KeyCode::Char('s') => {
                self.context.scope = if self.context.scope == Scope::Branch {
                    Scope::NextRequest
                } else {
                    Scope::Branch
                };
                self.context.preview = None;
                self.context.display_cache = None;
                self.context.status = "Scope changed · preview before apply".into();
            }
            KeyCode::Char('a') if !self.read_only && self.context.preview.is_some() => {
                if let (Some(snapshot), Some(draft)) = (&self.context.snapshot, &self.context.draft)
                {
                    let edit = Apply {
                        revision: snapshot.revision.clone(),
                        document: draft.clone(),
                        scope: self.context.scope,
                        source: "tui".into(),
                    };
                    let body = json!({ "request_id": self.id(), "edit": edit });
                    self.context.pending = Some(body.clone());
                    self.context.waiting = true;
                    self.context.status = "Applying reviewed draft…".into();
                    self.dispatch("/context/apply", body);
                }
            }
            KeyCode::Up => self.context.selected = self.context.selected.saturating_sub(1),
            KeyCode::Down => {
                if let Some(draft) = &self.context.draft {
                    self.context.selected = (self.context.selected + 1).min(draft.entries.len());
                }
            }
            KeyCode::Char('J') if !self.read_only => self.context_edit(Editing::Document),
            KeyCode::Enter | KeyCode::Char('e' | 'j') if !self.read_only => {
                if let Some(draft) = &self.context.draft {
                    let index = self.context.selected;
                    self.context_edit(if index == draft.entries.len() {
                        Editing::Tools
                    } else if key.code == KeyCode::Char('j') {
                        Editing::Item(index)
                    } else {
                        Editing::Text(index)
                    });
                }
            }
            KeyCode::Char('n') if !self.read_only => {
                let id = format!("inserted:{}", self.id());
                if let Some(draft) = &mut self.context.draft {
                    let index = self.context.selected.min(draft.entries.len());
                    draft.entries.insert(
                        index,
                        Entry {
                            references: vec![],
                            id,
                            item: Item::Message {
                                role: "user".into(),
                                content: vec![Block::Text {
                                    text: String::new(),
                                }],
                            },
                        },
                    );
                    self.context.invalidate();
                    self.context_edit(Editing::Text(index));
                }
            }
            KeyCode::Char('x' | '[' | ']' | 'v') if !self.read_only => {
                let index = self.context.selected;
                if let Some(draft) = &mut self.context.draft
                    && index < draft.entries.len()
                {
                    match key.code {
                        KeyCode::Char('x') => {
                            draft.entries.remove(index);
                            self.context.selected = index.min(draft.entries.len());
                        }
                        KeyCode::Char('[') if index > 0 => {
                            draft.entries.swap(index, index - 1);
                            self.context.selected -= 1;
                        }
                        KeyCode::Char(']') if index + 1 < draft.entries.len() => {
                            draft.entries.swap(index, index + 1);
                            self.context.selected += 1;
                        }
                        KeyCode::Char('v') => {
                            if let Item::Message { role, .. } = &mut draft.entries[index].item {
                                *role = match role.as_str() {
                                    "user" => "assistant",
                                    "assistant" => "system",
                                    "system" => "developer",
                                    _ => "user",
                                }
                                .into();
                            }
                        }
                        _ => {}
                    }
                    self.context.invalidate();
                }
            }
            _ => {}
        }
    }
}

impl App {
    pub(crate) fn open_context_management(&mut self, rebuild: bool) {
        self.open_context();
        if self.read_only
            || self.snapshot.state.active_run.is_some()
            || self.snapshot.state.managing
        {
            self.context.status =
                "Context management requires an idle writable session · draft retained".into();
            return;
        }
        if self.context.images.pending.is_some()
            || self.context.pending.is_some()
            || self.context.operation.is_some()
        {
            self.context.status = "Finish the pending context operation first".into();
            return;
        }
        let Some(snapshot) = &self.context.snapshot else {
            self.context.open_management_after_inspect = Some(rebuild);
            return;
        };
        let edits = self
            .snapshot
            .history
            .iter()
            .filter(|r| r.kind == "context_edit" && r.payload["scope"] == "branch")
            .map(|r| {
                (
                    r.sequence,
                    r.payload["source"]
                        .as_str()
                        .unwrap_or("unknown source")
                        .to_owned(),
                    true,
                )
            })
            .collect();
        self.context.management = Some(ManagementDraft {
            rebuild,
            branch: format!("rebuilt-{}", snapshot.revision.sequence),
            instructions: String::new(),
            edits,
            selected: 0,
        });
        self.context.status = if rebuild {
            "Rebuild uses original records and selected saved edits; no model call or tool replay. \
             Unsaved draft is not applied."
        } else {
            "Compaction calls the configured model policy. Unsaved context draft is not applied."
        }
        .into();
    }
    fn context_management_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.start_context_operation();
            return;
        }
        let Some(management) = &mut self.context.management else {
            return;
        };
        let last = if management.rebuild {
            management.edits.len() + 1
        } else {
            1
        };
        match key.code {
            KeyCode::Up => management.selected = management.selected.saturating_sub(1),
            KeyCode::Down => management.selected = (management.selected + 1).min(last),
            KeyCode::Char(' ')
                if management.rebuild && management.selected > 0 && management.selected < last =>
            {
                management.edits[management.selected - 1].2 =
                    !management.edits[management.selected - 1].2;
            }
            KeyCode::Enter if management.selected == last => self.start_context_operation(),
            KeyCode::Enter if management.selected == 0 => {
                let editing = if management.rebuild {
                    Editing::Branch
                } else {
                    Editing::Instructions
                };
                self.context_edit(editing);
            }
            _ => {}
        }
    }
    fn start_context_operation(&mut self) {
        if self.read_only
            || self.snapshot.state.active_run.is_some()
            || self.snapshot.state.managing
        {
            self.context.status =
                "Session became busy · operation options and context draft retained".into();
            return;
        }
        let (Some(management), Some(snapshot)) = (&self.context.management, &self.context.snapshot)
        else {
            return;
        };
        let (route, body) = if management.rebuild {
            if management.branch.trim().is_empty() {
                self.context.status = "A new branch name is required".into();
                return;
            }
            let rebuild = eden_protocol::context_edit::Rebuild {
                revision: snapshot.revision.clone(),
                branch: management.branch.clone(),
                edit_ids: management
                    .edits
                    .iter()
                    .filter(|e| e.2)
                    .map(|e| e.0)
                    .collect(),
            };
            (
                "/context/rebuild",
                json!({ "request_id": self.id(), "rebuild": rebuild }),
            )
        } else {
            (
                "/context/compact",
                json!({ "request_id": self.id(), "instructions": management.instructions }),
            )
        };
        self.context.operation = Some(Operation {
            route,
            body: body.clone(),
            run: None,
        });
        self.context.waiting = true;
        self.context.status =
            "Submitting management operation · acceptance is not completion".into();
        self.dispatch(route, body);
    }
    fn retry_context_operation(&mut self) {
        let Some(operation) = &self.context.operation else {
            return;
        };
        self.context.waiting = true;
        if let Some(run_id) = operation.run {
            self.dispatch(
                "/terminal",
                json!({ "run_id": run_id, "context_operation": operation.body["request_id"] }),
            );
        } else {
            self.dispatch(operation.route, operation.body.clone());
        }
    }
    fn context_operation_reply(&mut self, route: &str, body: &Value, result: Result<Value, Fault>) {
        let Some(operation) = &self.context.operation else {
            return;
        };
        let expected = &operation.body["request_id"];
        let matches = if route == "/terminal" {
            body["context_operation"] == *expected && body["run_id"].as_u64() == operation.run
        } else {
            route == operation.route && body["request_id"] == *expected
        };
        if !matches {
            return;
        }
        self.context.waiting = false;
        if route != "/terminal" {
            match result {
                Ok(value) => {
                    if let Some(run_id) = value["run_id"].as_u64() {
                        if let Some(operation) = &mut self.context.operation {
                            operation.run = Some(run_id);
                        }
                        self.context.management = None;
                        self.context.status = format!(
                            "Operation #{run_id} accepted · waiting for completion; context draft \
                             retained"
                        );
                        self.retry_context_operation();
                    } else {
                        self.context.status =
                            "Admission receipt unreadable · t retries the same operation".into();
                    }
                }
                Err(error) => {
                    if error.source != "live-client" {
                        self.context.operation = None;
                    }
                    self.context.status = format!(
                        "Operation not confirmed · options and context draft retained: {error}"
                    );
                }
            }
            return;
        }
        let terminal = result.and_then(|value| {
            serde_json::from_value::<eden_protocol::Terminal>(value)
                .map_err(|e| Fault::new("InputFailure", "live-client", e.to_string()))
        });
        match terminal {
            Err(error) => {
                self.context.status = format!(
                    "Completion unknown · t waits for the same run; context draft retained: \
                     {error}"
                )
            }
            Ok(terminal) => {
                self.context.operation = None;
                self.context.view = 5;
                self.context.display_cache = None;
                self.context.refreshed = None;
                match terminal.into_result() {
                    Ok(value) => {
                        self.context.operation_result =
                            format!("Operation completed\n{}", pretty(&value));
                        self.context.refresh_after_operation = true;
                        self.inspect_context(false);
                    }
                    Err(error) => {
                        self.context.operation_result =
                            format!("Operation failed · context draft retained\n{error}");
                        self.context.status = self.context.operation_result.clone();
                    }
                }
            }
        }
    }
}
fn render_management(management: &ManagementDraft, buf: &mut Buffer, area: Rect, p: Palette) {
    let mut rows = vec![if management.rebuild {
        format!("New branch: {}", management.branch)
    } else {
        format!(
            "Instructions: {}",
            if management.instructions.is_empty() {
                "(use configured defaults)"
            } else {
                &management.instructions
            }
        )
    }];
    if management.rebuild {
        rows.extend(management.edits.iter().map(|(id, source, included)| {
            format!(
                "[{}] Edit #{id} · {source}",
                if *included { "x" } else { " " }
            )
        }));
    }
    rows.push(
        if management.rebuild {
            "Start rebuild · no model call / no tool replay"
        } else {
            "Start compaction · calls configured model policy"
        }
        .into(),
    );
    let start = management
        .selected
        .saturating_sub(area.height.saturating_sub(1) as usize);
    for (row, (index, text)) in rows
        .iter()
        .enumerate()
        .skip(start)
        .take(area.height as usize)
        .enumerate()
    {
        Paragraph::new(format!(
            "{} {text}",
            if index == management.selected {
                ">"
            } else {
                " "
            }
        ))
        .style(if index == management.selected {
            p.style().fg(p.accent).add_modifier(Modifier::BOLD)
        } else {
            p.style()
        })
        .render(Rect::new(area.x, area.y + row as u16, area.width, 1), buf);
    }
}

pub fn render(
    app: &mut App,
    buf: &mut Buffer,
    area: Rect,
    p: Palette,
    mono: bool,
) -> (Geometry, Option<(u16, u16)>) {
    let inner = Rect::new(
        area.x + 1,
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );
    let mut g = Geometry {
        modal: Some(area),
        ..Default::default()
    };
    let title = format!(
        "Context · {} · {}{}",
        if let Some(management) = &app.context.management {
            if management.rebuild {
                "Rebuild options"
            } else {
                "Compaction options"
            }
        } else {
            [
                "Effective draft",
                "Original",
                "Preview diff",
                "Last actual request",
                "Policies",
                "Operation result",
                "Model budget",
                "Images",
            ][app.context.view]
        },
        if app.context.scope == Scope::Branch {
            "branch"
        } else {
            "next logical request"
        },
        if app.read_only { " · read only" } else { "" }
    );
    Paragraph::new(title)
        .style(p.style().fg(p.heading).add_modifier(Modifier::BOLD))
        .render(Rect::new(inner.x, inner.y, inner.width, 1), buf);
    let status_area = Rect::new(inner.x, inner.bottom().saturating_sub(6), inner.width, 2);
    Paragraph::new(app.context.status.as_str())
        .style(p.style().fg(p.warning))
        .wrap(Wrap { trim: false })
        .render(status_area, buf);
    let help = if app.context.editing.is_some() {
        "Ctrl+S save field · Esc cancel field · paste inserts text"
    } else if app.context.view == 7 {
        "↑↓ image · o original/sent · 1 preserve · 2 omit · 3 re-adapt\np review · a apply · b \
         rebase · r reload · t retry\nTab views · B budget · Esc close"
    } else if app.context.management.is_some() {
        "↑↓ select · Enter edit / Start · Space toggle edit · Ctrl+S Start · Esc close form"
    } else {
        "↑↓ select · e edit · j/J JSON · n add · x exclude · [ ] move · v role\ns scope · p \
         preview · a apply · Tab views · PgUp/Dn scroll\nb rebase · r reload · t retry · Esc \
         close\nR rebuild · c compact · P policies · B budget · I images · g configure"
    };
    Paragraph::new(help)
        .style(p.style().fg(p.muted))
        .wrap(Wrap { trim: false })
        .render(
            Rect::new(inner.x, inner.bottom().saturating_sub(4), inner.width, 4),
            buf,
        );
    let content = Rect::new(
        inner.x,
        inner.y + 2,
        inner.width,
        inner.height.saturating_sub(8),
    );
    if app.context.editing.is_some() {
        g.field_editor = Some(content);
        let cursor = app
            .field_editor
            .draw(buf, content, 0, !app.preferences.light, mono);
        return (g, cursor);
    }
    if let Some(management) = &app.context.management {
        render_management(management, buf, content, p);
        g.transcript = content;
        return (g, None);
    }
    let Some(snapshot) = &app.context.snapshot else {
        return (g, None);
    };
    Paragraph::new(format!(
        "Revision {} · {} · {}",
        snapshot.revision.sequence, snapshot.revision.branch, app.context.usage
    ))
    .style(p.style().fg(p.muted))
    .render(Rect::new(inner.x, inner.y + 1, inner.width, 1), buf);
    if app.context.view == 7 {
        g.image = images::render(app, buf, content, p, mono);
        return (g, None);
    }
    let display = app.context.display();
    let text = match app.context.view {
        1..=6 => display.as_ref(),
        _ => {
            let Some(draft) = &app.context.draft else {
                return (g, None);
            };
            let list_height = (content.height / 3).clamp(1, 8);
            let start = app
                .context
                .selected
                .saturating_sub(list_height.saturating_sub(1) as usize);
            for (row, index) in (start..=draft.entries.len())
                .take(list_height as usize)
                .enumerate()
            {
                let label = if let Some(entry) = draft.entries.get(index) {
                    let kind = match &entry.item {
                        Item::Message { role, .. } => role.as_str(),
                        Item::ToolCall { .. } => "tool call",
                        Item::ToolResult { .. } => "tool result",
                        Item::ProviderState { .. } => "provider state",
                    };
                    format!(
                        "{} {} · {}",
                        if index == app.context.selected {
                            ">"
                        } else {
                            " "
                        },
                        entry.id,
                        kind
                    )
                } else {
                    format!(
                        "{} Tool declarations ({})",
                        if index == app.context.selected {
                            ">"
                        } else {
                            " "
                        },
                        draft.tools.len()
                    )
                };
                Paragraph::new(label)
                    .style(if index == app.context.selected {
                        p.style().fg(p.accent).add_modifier(Modifier::BOLD)
                    } else {
                        p.style()
                    })
                    .render(
                        Rect::new(content.x, content.y + row as u16, content.width, 1),
                        buf,
                    );
            }
            g.transcript = Rect::new(content.x, content.y, content.width, list_height);
            let detail = display.as_ref();
            let detail_area = Rect::new(
                content.x,
                content.y + list_height + 1,
                content.width,
                content.height.saturating_sub(list_height + 1),
            );
            Paragraph::new(detail)
                .style(p.style())
                .wrap(Wrap { trim: false })
                .scroll((app.context.scroll.min(u16::MAX as usize) as u16, 0))
                .render(detail_area, buf);
            return (g, None);
        }
    };
    Paragraph::new(text)
        .style(p.style())
        .wrap(Wrap { trim: false })
        .scroll((app.context.scroll.min(u16::MAX as usize) as u16, 0))
        .render(content, buf);
    (g, None)
}

pub fn mouse(app: &mut App, g: &Geometry, event: MouseEvent) -> bool {
    match event.kind {
        MouseEventKind::ScrollDown => app.context.scroll = app.context.scroll.saturating_add(3),
        MouseEventKind::ScrollUp => app.context.scroll = app.context.scroll.saturating_sub(3),
        MouseEventKind::Down(MouseButton::Left) => {
            if app.context.editing.is_none()
                && let Some(management) = &mut app.context.management
            {
                if g.transcript.contains((event.column, event.row).into()) {
                    let last = if management.rebuild {
                        management.edits.len() + 1
                    } else {
                        1
                    };
                    let start = management
                        .selected
                        .saturating_sub(g.transcript.height.saturating_sub(1) as usize);
                    management.selected = (start + (event.row - g.transcript.y) as usize).min(last);
                }
                return true;
            }
            if let Some(area) = g
                .field_editor
                .filter(|r| r.contains((event.column, event.row).into()))
            {
                let _ = app.field_editor.mouse(
                    event.column - area.x,
                    event.row - area.y,
                    event.modifiers.contains(KeyModifiers::SHIFT),
                );
            } else if g.transcript.contains((event.column, event.row).into())
                && let Some(draft) = &app.context.draft
            {
                let start = app
                    .context
                    .selected
                    .saturating_sub(g.transcript.height.saturating_sub(1) as usize);
                app.context.selected =
                    (start + (event.row - g.transcript.y) as usize).min(draft.entries.len());
                app.context.scroll = 0;
            }
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(sequence: u64, entries: Value) -> Snapshot {
        serde_json::from_value(json!({
            "revision": {
                "session_id": 7,
                "sequence": sequence,
                "head": sequence,
                "branch": "main",
            },
            "original": { "entries": entries, "tools": [] },
            "effective": { "entries": entries, "tools": [] },
            "edits": [],
            "last_request": null,
        }))
        .unwrap()
    }
    fn entry(id: &str, text: &str) -> Value {
        json!({
            "id": id,
            "item": {
                "type": "message",
                "role": "user",
                "content": [{ "type": "text", "text": text }],
            },
        })
    }
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    #[test]
    fn explicit_rebase_preserves_edits_and_appended_input() {
        let mut state = Context::default();
        state.capture(snapshot(1, json!([entry("one", "old")])));
        state.draft.as_mut().unwrap().entries[0] =
            serde_json::from_value(entry("one", "edited")).unwrap();
        state
            .rebase(snapshot(
                2,
                json!([entry("one", "old"), entry("two", "new descendant")]),
            ))
            .unwrap();
        let draft = state.draft.as_ref().unwrap();
        assert_eq!(draft.entries.len(), 2);
        assert!(pretty(draft).contains("edited"));
        assert_eq!(state.snapshot.unwrap().revision.sequence, 2);
    }
    #[test]
    fn failed_rebase_keeps_original_revision_and_draft() {
        let mut state = Context::default();
        state.capture(snapshot(1, json!([entry("one", "old")])));
        state.draft.as_mut().unwrap().entries[0] =
            serde_json::from_value(entry("one", "edited")).unwrap();
        assert!(state.rebase(snapshot(2, json!([]))).is_err());
        assert_eq!(state.snapshot.as_ref().unwrap().revision.sequence, 1);
        assert!(pretty(state.draft.as_ref().unwrap()).contains("edited"));
    }
    #[test]
    fn incomplete_tool_groups_cannot_be_previewed_for_apply() {
        let mut state = Context::default();
        state.capture(snapshot(
            1,
            json!([{
                "id": "call",
                "item": { "type": "tool_call", "call_id": "c", "name": "read", "arguments": "{}" },
            }]),
        ));
        assert!(state.preview().is_err());
        assert!(state.preview.is_none());
    }
    #[tokio::test]
    async fn context_editor_and_conflicts_preserve_independent_composer_and_draft() {
        let mut app = crate::app::tests::app();
        app.editor.restore("unsent composer", 0).unwrap();
        app.context
            .capture(snapshot(1, json!([entry("one", "old")])));
        app.open_context();
        app.key(key(KeyCode::Char('e')));
        app.field_editor.restore("local context", 0).unwrap();
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        app.key(key(KeyCode::Char('p')));
        assert!(app.context.preview.is_some());
        app.key(key(KeyCode::Char('a')));
        let body = app.context.pending.clone().unwrap();
        app.context_reply(
            "/context/apply",
            &body,
            Err(Fault::new(
                "ContextConflict",
                "context-edit",
                "stale revision",
            )),
        );
        assert!(app.context.pending.is_none());
        assert!(pretty(app.context.draft.as_ref().unwrap()).contains("local context"));
        app.key(key(KeyCode::Esc));
        app.open_context();
        assert!(pretty(app.context.draft.as_ref().unwrap()).contains("local context"));
        assert_eq!(app.editor.text(), "unsent composer");
    }
    #[tokio::test]
    async fn unknown_apply_is_retried_with_same_id_and_late_receipts_cannot_replace_draft() {
        let mut app = crate::app::tests::app();
        app.context
            .capture(snapshot(1, json!([entry("one", "old")])));
        app.open_context();
        app.key(key(KeyCode::Char('p')));
        app.key(key(KeyCode::Char('a')));
        let body = app.context.pending.clone().unwrap();
        app.context_reply(
            "/context/apply",
            &body,
            Err(Fault::new("InputFailure", "live-client", "reply lost")),
        );
        assert_eq!(
            app.context.pending.as_ref().unwrap()["request_id"],
            body["request_id"]
        );
        app.key(key(KeyCode::Char('x')));
        assert_eq!(app.context.draft.as_ref().unwrap().entries.len(), 1);
        app.key(key(KeyCode::Char('t')));
        assert_eq!(
            app.context.pending.as_ref().unwrap()["request_id"],
            body["request_id"]
        );
        app.context_reply(
            "/context/apply",
            &json!({ "request_id": "other" }),
            Ok(serde_json::to_value(snapshot(8, json!([]))).unwrap()),
        );
        assert_eq!(app.context.snapshot.as_ref().unwrap().revision.sequence, 1);
        app.context_reply(
            "/context/apply",
            &body,
            Ok(serde_json::to_value(snapshot(2, json!([entry("one", "applied")]))).unwrap()),
        );
        assert!(app.context.pending.is_none());
        assert_eq!(app.context.snapshot.as_ref().unwrap().revision.sequence, 2);
    }
    #[tokio::test]
    async fn modal_paste_and_reload_confirmation_never_modify_composer() {
        let mut app = crate::app::tests::app();
        app.editor.restore("composer", 0).unwrap();
        app.context
            .capture(snapshot(1, json!([entry("one", "old")])));
        app.open_context();
        app.paste("/quit\n");
        assert_eq!(app.editor.text(), "composer");
        app.key(key(KeyCode::Char('x')));
        app.key(key(KeyCode::Char('r')));
        app.key(key(KeyCode::Char('n')));
        assert!(app.context.draft.as_ref().unwrap().entries.is_empty());
        assert!(app.context.inspect.is_none());
        for (width, height) in [(40, 12), (80, 24), (160, 48), (8, 5), (0, 0)] {
            let rect = Rect::new(0, 0, width, height);
            crate::view::render(&mut app, &mut Buffer::empty(rect), rect, true);
        }
    }
    #[test]
    fn usage_comes_from_actual_request_claims() {
        let records: Vec<eden_protocol::coding::Record> = serde_json::from_value(json!([
            {
                "sequence": 1,
                "schema_version": 1,
                "session_id": 7,
                "run_id": 0,
                "kind": "context_edit",
                "payload": {},
            },
            {
                "sequence": 2,
                "schema_version": 1,
                "session_id": 7,
                "run_id": 0,
                "kind": "context_edit",
                "payload": {},
            },
            {
                "sequence": 3,
                "schema_version": 1,
                "session_id": 7,
                "run_id": 9,
                "kind": "model_request",
                "payload": { "context_edits": [1] },
            }
        ]))
        .unwrap();
        let mut state = Context::default();
        state.update_usage(&records);
        assert_eq!(state.usage, "#1 used · #2 pending");
    }
    #[tokio::test]
    async fn rebuild_selects_only_persistent_edits_and_keeps_context_draft() {
        let mut app = crate::app::tests::app();
        app.context
            .capture(snapshot(4, json!([entry("one", "original")])));
        app.context.draft.as_mut().unwrap().entries[0] =
            serde_json::from_value(entry("one", "unsaved draft")).unwrap();
        app.snapshot.history = serde_json::from_value(json!([
            {
                "schema_version": 1,
                "session_id": 7,
                "sequence": 2,
                "run_id": 0,
                "kind": "context_edit",
                "payload": { "scope": "branch", "source": "tui" },
            },
            {
                "schema_version": 1,
                "session_id": 7,
                "sequence": 3,
                "run_id": 0,
                "kind": "context_edit",
                "payload": { "scope": "next_request", "source": "sdk" },
            }
        ]))
        .unwrap();
        app.open_context_management(true);
        let form = app.context.management.as_ref().unwrap();
        assert_eq!(form.branch, "rebuilt-4");
        assert_eq!(form.edits.len(), 1);
        app.context_management_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let body = app.context.operation.as_ref().unwrap().body.clone();
        assert_eq!(body["rebuild"]["edit_ids"], json!([2]));
        app.context_reply("/context/rebuild", &body, Ok(json!({ "run_id": 81 })));
        assert_eq!(app.context.operation.as_ref().unwrap().run, Some(81));
        assert!(!app.context.refresh_after_operation);
        app.context_reply(
            "/terminal",
            &json!({ "run_id": 81, "context_operation": body["request_id"] }),
            Ok(json!({
                "outcome": { "status": "completed", "value": { "branch": "rebuilt-4" } },
                "cleanup_errors": [],
            })),
        );
        assert!(app.context.operation.is_none());
        assert!(app.context.refresh_after_operation);
        let id = app.context.inspect.as_ref().unwrap().0.clone();
        app.context_reply(
            "/context/inspect",
            &json!({ "request_id": id }),
            Ok(serde_json::to_value(snapshot(5, json!([entry("one", "rebuilt result")]))).unwrap()),
        );
        assert_eq!(app.context.view, 5);
        assert!(app.context.display().contains("rebuilt result"));
        assert!(pretty(app.context.draft.as_ref().unwrap()).contains("unsaved draft"));
        assert_eq!(app.context.snapshot.as_ref().unwrap().revision.sequence, 4);
    }
    #[tokio::test]
    async fn compaction_failure_is_distinct_from_admission_and_keeps_draft() {
        let mut app = crate::app::tests::app();
        app.context
            .capture(snapshot(1, json!([entry("one", "retained")])));
        app.open_context_management(false);
        app.context_management_key(key(KeyCode::Enter));
        app.field_editor
            .restore("Keep user requirements", 0)
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(app.context.operation.is_none());
        app.context_management_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let body = app.context.operation.as_ref().unwrap().body.clone();
        assert_eq!(body["instructions"], "Keep user requirements");
        app.context_reply("/context/compact", &body, Ok(json!({ "run_id": 82 })));
        app.context_reply(
            "/terminal",
            &json!({ "run_id": 82, "context_operation": body["request_id"] }),
            Ok(
                serde_json::to_value(eden_protocol::Terminal::failed(Fault::new(
                    "PolicyFailed",
                    "policy",
                    "configured policy failed",
                )))
                .unwrap(),
            ),
        );
        assert!(app.context.operation.is_none());
        assert!(app.context.inspect.is_none());
        assert!(
            app.context
                .operation_result
                .contains("configured policy failed")
        );
        assert!(pretty(app.context.draft.as_ref().unwrap()).contains("retained"));
    }
    #[tokio::test]
    async fn management_retries_original_admission_and_waits_for_same_run() {
        let mut app = crate::app::tests::app();
        app.context.capture(snapshot(1, json!([])));
        app.open_context_management(false);
        app.start_context_operation();
        let body = app.context.operation.as_ref().unwrap().body.clone();
        app.context_reply(
            "/context/compact",
            &body,
            Err(Fault::new("InputFailure", "live-client", "lost receipt")),
        );
        app.key(key(KeyCode::Char('t')));
        assert_eq!(app.context.operation.as_ref().unwrap().body, body);
        app.context_reply("/context/compact", &body, Ok(json!({ "run_id": 83 })));
        let wait = json!({ "run_id": 83, "context_operation": body["request_id"] });
        app.context_reply(
            "/terminal",
            &wait,
            Err(Fault::new("Unavailable", "live-client", "wait timeout")),
        );
        app.key(key(KeyCode::Char('t')));
        assert_eq!(app.context.operation.as_ref().unwrap().run, Some(83));
        assert_eq!(app.context.operation.as_ref().unwrap().body, body);
    }
    #[tokio::test]
    async fn policies_show_configured_order_and_busy_management_stays_local() {
        let mut app = crate::app::tests::app();
        let mut captured = snapshot(1, json!([]));
        captured.policies = serde_json::from_value(json!([
            {
                "name": "first",
                "role": "large_tool_output",
                "boundary": "tool_round_end",
                "enabled": false,
                "config": {},
            },
            {
                "name": "second",
                "role": "custom",
                "boundary": "before_request",
                "enabled": true,
                "config": {},
            }
        ]))
        .unwrap();
        app.context.capture(captured);
        app.context.view = 4;
        let display = app.context.display();
        assert!(display.find("1. first").unwrap() < display.find("2. second").unwrap());
        assert!(display.contains("disabled"));
        app.snapshot.state.active_run = Some(9);
        app.open_context_management(true);
        assert!(app.context.management.is_none());
        assert!(app.context.operation.is_none());
    }
}
