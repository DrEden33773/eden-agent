//! Source previews freeze into owned draft material; navigation never switches the live session.
use crate::{
    app::App,
    autocomplete::Replacement,
    view::{Geometry, Palette},
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use eden_protocol::{
    Fault,
    coding::{Block, Item},
    session_reference::{
        self as reference, Branch, CatalogEntry, ImageSelection, Preview, Selection,
    },
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Modifier,
    widgets::{Paragraph, Widget, Wrap},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{Arc, mpsc},
};
use unicode_segmentation::UnicodeSegmentation;
#[derive(Clone, Copy, Default, PartialEq)]
enum Page {
    #[default]
    Catalog,
    Branches,
    Preview,
    Attached,
}
struct Row {
    label: String,
    detail: String,
    detail_lines: Vec<String>,
    entry: Option<String>,
    image: Option<ImageSelection>,
    tokens: u64,
}
#[derive(Default)]
pub struct Picker {
    pub open: bool,
    page: Page,
    catalog: Vec<CatalogEntry>,
    branches: Vec<Branch>,
    path: String,
    preview: Option<Arc<Preview>>,
    rows: Vec<Row>,
    selected: usize,
    scroll: usize,
    entries: BTreeSet<String>,
    images: BTreeSet<ImageSelection>,
    pending: Option<(String, String)>,
    budget_request: Option<String>,
    budget: Option<Arc<eden_protocol::context_edit::Snapshot>>,
    available: Option<u64>,
    selected_estimate: u64,
    system_detail: String,
    system_lines: Vec<String>,
    width: usize,
    prepared_width: usize,
    requested_width: usize,
    comparison: String,
    show_system: bool,
    replacement: Option<Replacement>,
    status: String,
    query: String,
    visible: Vec<usize>,
    expected_source: Option<String>,
    generation: u64,
    applied_generation: u64,
    rows_generation: u64,
    preparing: bool,
    worker: Option<(mpsc::Sender<Work>, mpsc::Receiver<Prepared>)>,
    #[cfg(test)]
    worker_thread: Option<std::thread::ThreadId>,
}
#[derive(Clone, Copy)]
enum WorkKind {
    Preview,
    Budget,
    Attached,
    Freeze,
    Layout,
}
struct Work {
    kind: WorkKind,
    value: Value,
    source: Option<String>,
    generation: u64,
    composer: String,
    references: Vec<Arc<reference::Reference>>,
    preview: Option<Arc<Preview>>,
    width: usize,
}
struct PreparedRows {
    page: Page,
    rows: Vec<Row>,
    entries: BTreeSet<String>,
    images: BTreeSet<ImageSelection>,
    system_detail: String,
    system_lines: Vec<String>,
    estimate: u64,
    width: usize,
    reflow: bool,
}
struct Prepared {
    generation: u64,
    source: Option<String>,
    preview: Option<Arc<Preview>>,
    budget: Option<Arc<eden_protocol::context_edit::Snapshot>>,
    rows: Option<PreparedRows>,
    available: Option<u64>,
    comparison: String,
    error: Option<String>,
    frozen: Option<Arc<reference::Reference>>,
    #[cfg(test)]
    thread: std::thread::ThreadId,
}
fn prepare_work(state: &mut Picker, work: Work) -> Prepared {
    let mut rows = None;
    let mut frozen = None;
    state.width = work.width;
    let result: Result<(), String> = (|| {
        match work.kind {
            WorkKind::Preview => {
                state.expected_source = work.source;
                state.preview_rows(serde_json::from_value(work.value).map_err(|e| e.to_string())?);
            }
            WorkKind::Budget => {
                state.budget = Some(Arc::new(
                    serde_json::from_value(work.value).map_err(|e| e.to_string())?,
                ))
            }
            WorkKind::Attached => {
                state.attached_rows(&work.references);
                state.page = Page::Attached;
            }
            WorkKind::Layout => {
                if state.page == Page::Attached {
                    state.attached_rows(&work.references);
                } else if let Some(preview) = state.preview.clone() {
                    state.preview_rows((*preview).clone());
                }
            }
            WorkKind::Freeze => {
                let preview = work.preview.as_ref().ok_or("Source preview missing")?;
                state.preview = Some(preview.clone());
                let selection: Selection = serde_json::from_value(work.value["selection"].clone())
                    .map_err(|e| e.to_string())?;
                frozen = Some(Arc::new(
                    preview
                        .freeze(
                            work.value["id"].as_str().unwrap_or_default().into(),
                            &selection,
                        )
                        .map_err(|e| e.to_string())?,
                ));
                state.expected_source = work.source;
            }
        }
        if matches!(work.kind, WorkKind::Preview | WorkKind::Budget) {
            state.recalculate_budget(&work.composer, &work.references);
            state.update_comparison();
        }
        if matches!(
            work.kind,
            WorkKind::Preview | WorkKind::Attached | WorkKind::Layout
        ) {
            rows = Some(PreparedRows {
                page: state.page,
                rows: std::mem::take(&mut state.rows),
                entries: std::mem::take(&mut state.entries),
                images: std::mem::take(&mut state.images),
                system_detail: std::mem::take(&mut state.system_detail),
                system_lines: std::mem::take(&mut state.system_lines),
                estimate: state.selected_estimate,
                width: work.width,
                reflow: matches!(work.kind, WorkKind::Layout),
            });
        }
        Ok(())
    })();
    Prepared {
        generation: work.generation,
        source: state.expected_source.clone(),
        preview: state.preview.clone(),
        budget: state.budget.clone(),
        rows,
        available: state.available,
        comparison: state.comparison.clone(),
        error: result.err(),
        frozen,
        #[cfg(test)]
        thread: std::thread::current().id(),
    }
}

fn wrap_detail(text: &str, width: usize) -> Vec<String> {
    text.lines()
        .flat_map(|line| {
            crate::text::wrap(line, width)
                .into_iter()
                .map(|(_, text)| text)
        })
        .collect()
}
fn blocks(item: &Item) -> &[Block] {
    match item {
        Item::Message { content, .. } => content,
        Item::ToolResult { result, .. } => &result.content,
        _ => &[],
    }
}
fn block_text(blocks: &[Block]) -> String {
    blocks
        .iter()
        .map(|b| match b {
            Block::Text { text } => text.clone(),
            Block::Image { media_type, data } => format!(
                "[Image: {media_type}, {} encoded bytes; snapshot]",
                data.len()
            ),
            Block::File {
                name, media_type, ..
            } => format!("[File: {name}, {media_type}; binary excluded]"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn item_text(item: &Item) -> String {
    match item {
        Item::Message { role, content } => format!("{role}\n{}", block_text(content)),
        Item::ToolCall {
            name,
            arguments,
            call_id,
        } => format!("Quoted tool call {name} ({call_id})\n{arguments}"),
        Item::ToolResult { result, call_id } => format!(
            "Quoted tool result {call_id}\n{}\n{}",
            result.text,
            block_text(&result.content)
        ),
        Item::ProviderState { .. } => "Provider state is not selectable".into(),
    }
}
impl Picker {
    fn row_index(&self) -> usize {
        if self.page == Page::Catalog {
            self.visible
                .get(self.selected)
                .copied()
                .unwrap_or(usize::MAX)
        } else {
            self.selected
        }
    }
    fn row_count(&self) -> usize {
        if self.page == Page::Catalog {
            self.visible.len()
        } else {
            self.rows.len()
        }
    }
    fn filter_catalog(&mut self) {
        let selected = self.visible.get(self.selected).copied();
        let query = self.query.to_lowercase();
        self.visible = self
            .catalog
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                query.is_empty()
                    || entry.source.label.to_lowercase().contains(&query)
                    || entry.source.path.to_lowercase().contains(&query)
                    || entry.source.branch.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = selected
            .and_then(|index| self.visible.iter().position(|visible| *visible == index))
            .unwrap_or(0);
        self.scroll = 0;
    }

    fn sanitize_rows(&mut self) {
        for row in &mut self.rows {
            row.label = crate::text::clean(&row.label);
            row.detail = crate::text::clean(&row.detail);
            row.detail_lines = wrap_detail(&row.detail, self.width.max(1));
        }
    }
    fn update_estimate(&mut self) {
        self.selected_estimate = self
            .rows
            .iter()
            .filter(|row| match (&row.entry, &row.image) {
                (Some(id), _) => self.entries.contains(id),
                (_, Some(image)) => self.images.contains(image),
                _ => false,
            })
            .map(|row| row.tokens)
            .sum();
    }
    fn preview_rows(&mut self, preview: Preview) {
        self.rows.clear();
        self.entries.clear();
        self.images.clear();
        self.selected = 0;
        self.scroll = 0;
        for entry in preview
            .entries
            .iter()
            .filter(|e| reference::selectable(&e.item))
        {
            let detail = item_text(&entry.item);
            self.entries.insert(entry.id.clone());
            self.rows.push(Row {
                detail_lines: vec![],
                label: format!(
                    "{} · {}",
                    entry.id,
                    crate::text::clipped(&detail.replace('\n', " "), 70)
                ),
                tokens: detail.chars().count() as u64 / 4 + 1,
                detail,
                entry: Some(entry.id.clone()),
                image: None,
            });
            for (index, block) in blocks(&entry.item)
                .iter()
                .enumerate()
                .filter(|(_, b)| matches!(b, Block::Image { .. }))
            {
                self.rows.push(Row {
                    detail_lines: vec![],
                    label: format!("Image {}[{index}] · explicit opt-in", entry.id),
                    detail: block_text(std::slice::from_ref(block)),
                    entry: None,
                    image: Some(ImageSelection {
                        entry_id: entry.id.clone(),
                        block_index: index,
                    }),
                    tokens: 1024,
                });
            }
        }
        self.system_detail = preview
            .source_system
            .as_ref()
            .map(|items| items.iter().map(item_text).collect::<Vec<_>>().join("\n\n"))
            .unwrap_or_else(|| {
                "Source system snapshot unknown; current source resources are not a substitute"
                    .into()
            });
        self.system_detail = crate::text::clean(&self.system_detail);
        self.system_lines = wrap_detail(&self.system_detail, self.width.max(1));
        self.sanitize_rows();
        self.preview = Some(Arc::new(preview));
        self.page = Page::Preview;
        self.update_estimate();
    }
    fn update_comparison(&mut self) {
        self.comparison = match (&self.preview, &self.budget) {
            (Some(preview), Some(budget)) => match &preview.source_system {
                Some(source) => {
                    let target: Vec<_> = budget.effective.entries.iter().filter(|entry| matches!(&entry.item, Item::Message{role,..} if role == "system" || role == "developer")).map(|e|e.item.clone()).collect();
                    if source == &target {"same as inspected target; rechecked at send"} else {"different from inspected target; rechecked at send"}
                }
                None => "unknown",
            },
            _ => "comparison unavailable",
        }.into();
    }
    fn recalculate_budget(
        &mut self,
        composer: &str,
        existing: &[std::sync::Arc<reference::Reference>],
    ) {
        self.available = self.budget.as_ref().and_then(|snapshot| {
            let window = snapshot.model.as_ref()?.limits.context_window;
            if window == 0 {
                return None;
            }
            let reserve = snapshot.budget["reserve_tokens"]["tokens"].as_u64()?;
            let used = snapshot
                .effective
                .entries
                .iter()
                .map(|e| item_text(&e.item).chars().count() as u64 / 4 + 1)
                .sum::<u64>()
                + serde_json::to_string(&snapshot.effective.tools)
                    .ok()?
                    .chars()
                    .count() as u64
                    / 4;
            let drafts = composer.chars().count() as u64 / 4
                + existing
                    .iter()
                    .map(|r| reference::estimate_blocks(&r.content))
                    .sum::<u64>();
            Some(window.saturating_sub(reserve.saturating_add(used).saturating_add(drafts)))
        });
    }
    fn attached_rows(&mut self, references: &[std::sync::Arc<reference::Reference>]) {
        self.rows = references
            .iter()
            .map(|r| Row {
                detail_lines: vec![],
                label: format!(
                    "{} · {} / {} @ {:?}",
                    r.id, r.source.label, r.source.branch, r.source.head
                ),
                detail: format!(
                    "Frozen reference · source system {} · {} selected images\n{}",
                    if r.source_system.is_some() {
                        "known"
                    } else {
                        "unknown"
                    },
                    r.included_images.len(),
                    block_text(&r.content)
                ),
                entry: None,
                image: None,
                tokens: 0,
            })
            .collect();
        self.sanitize_rows();
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }
}
impl App {
    fn queue_reference_work(&mut self, kind: WorkKind, value: Value, source: Option<String>) {
        if self.reference_picker.worker.is_none() {
            let (tx, rx) = mpsc::channel::<Work>();
            let (reply_tx, reply_rx) = mpsc::channel();
            tokio::task::spawn_blocking(move || {
                let mut state = Picker::default();
                while let Ok(work) = rx.recv() {
                    if reply_tx.send(prepare_work(&mut state, work)).is_err() {
                        break;
                    }
                }
            });
            self.reference_picker.worker = Some((tx, reply_rx));
        }
        self.reference_picker.generation += 1;
        if matches!(
            kind,
            WorkKind::Preview | WorkKind::Attached | WorkKind::Freeze
        ) {
            self.reference_picker.rows_generation = self.reference_picker.generation;
            self.reference_picker.preparing = true;
            self.reference_picker.status =
                "Preparing fixed source preview in background… · Esc cancels".into();
        }
        let work = Work {
            kind,
            value,
            source,
            generation: self.reference_picker.generation,
            composer: self.editor.text(),
            references: self.references.clone(),
            preview: self.reference_picker.preview.clone(),
            width: self.reference_picker.width.max(1),
        };
        if let Some((tx, _)) = &self.reference_picker.worker {
            let _ = tx.send(work);
        }
    }
    pub(crate) fn poll_references(&mut self) -> bool {
        if self.reference_picker.open
            && matches!(self.reference_picker.page, Page::Preview | Page::Attached)
            && !self.reference_picker.preparing
            && self.reference_picker.width != self.reference_picker.prepared_width
            && self.reference_picker.width != self.reference_picker.requested_width
        {
            self.reference_picker.requested_width = self.reference_picker.width;
            self.queue_reference_work(
                WorkKind::Layout,
                Value::Null,
                self.reference_picker.expected_source.clone(),
            );
        }
        let mut changed = false;
        while let Some(prepared) = self
            .reference_picker
            .worker
            .as_ref()
            .and_then(|(_, rx)| rx.try_recv().ok())
        {
            if !self.reference_picker.open
                || prepared.source != self.reference_picker.expected_source
                || prepared.generation < self.reference_picker.applied_generation
            {
                continue;
            }
            #[cfg(test)]
            {
                self.reference_picker.worker_thread = Some(prepared.thread);
            }
            self.reference_picker.applied_generation = prepared.generation;
            self.reference_picker.preview = prepared.preview;
            self.reference_picker.budget = prepared.budget;
            self.reference_picker.available = prepared.available;
            self.reference_picker.comparison = prepared.comparison;
            if let Some(reference) = prepared.frozen {
                self.reference_picker.preparing = false;
                self.accept_frozen_reference(reference);
                changed = true;
                continue;
            }
            if let Some(error) = prepared.error {
                self.reference_picker.status =
                    format!("Source preparation failed · composer retained: {error}");
                self.reference_picker.preparing = false;
            } else if let Some(rows) = prepared.rows
                && prepared.generation >= self.reference_picker.rows_generation
            {
                if rows.width != self.reference_picker.width {
                    self.reference_picker.requested_width = 0;
                    if rows.reflow {
                        continue;
                    }
                }
                self.reference_picker.rows = rows.rows;
                self.reference_picker.system_detail = rows.system_detail;
                self.reference_picker.system_lines = rows.system_lines;
                self.reference_picker.prepared_width = rows.width;
                self.reference_picker.page = rows.page;
                if !rows.reflow {
                    self.reference_picker.entries = rows.entries;
                    self.reference_picker.images = rows.images;
                    self.reference_picker.selected_estimate = rows.estimate;
                    self.reference_picker.preparing = false;
                    self.reference_picker.selected = 0;
                    self.reference_picker.scroll = 0;
                    self.reference_picker.status = "Preview fixed at captured source head · \
                                                    source changes cannot alter insertion"
                        .into();
                }
            }
            changed = true;
        }
        changed
    }

    pub(crate) fn open_references(&mut self, replacement: Option<Replacement>, attached: bool) {
        self.reference_picker = Picker {
            open: true,
            replacement,
            width: 96,
            ..Default::default()
        };
        self.autocomplete.dismiss();
        self.clipboard_generation += 1;
        if attached {
            self.reference_picker.page = Page::Attached;
            self.queue_reference_work(WorkKind::Attached, Value::Null, None);
            self.reference_picker.status =
                "Frozen draft references · x removes selected · n adds another".into();
        } else {
            self.reference_request("/session/catalog", json!({}));
        }
        let id = self.id();
        self.reference_picker.budget_request = Some(id.clone());
        self.dispatch(
            "/context/inspect",
            json!({ "request_id": id, "reference_budget": true }),
        );
    }
    fn reference_request(&mut self, route: &str, mut body: Value) {
        let id = self.id();
        body["request_id"] = json!(id);
        if route == "/reference/preview" {
            self.reference_picker.expected_source = Some(id.clone());
        }
        self.reference_picker.pending = Some((route.into(), id));
        self.reference_picker.status = "Loading source snapshot… · Esc cancels this picker".into();
        self.dispatch(route, body);
    }
    pub(crate) fn reference_reply(
        &mut self,
        route: &str,
        body: &Value,
        result: Result<Value, Fault>,
    ) {
        if !self.reference_picker.open {
            return;
        }
        if route == "/context/inspect" {
            if self.reference_picker.budget_request.as_deref() != body["request_id"].as_str() {
                return;
            }
            self.reference_picker.budget_request = None;
            if let Ok(value) = result {
                self.queue_reference_work(WorkKind::Budget, value, None);
            }
            return;
        }
        if !self
            .reference_picker
            .pending
            .as_ref()
            .is_some_and(|(r, id)| r == route && body["request_id"] == *id)
        {
            return;
        }
        self.reference_picker.pending = None;
        if route == "/reference/preview" {
            match result {
                Ok(value) => self.queue_reference_work(
                    WorkKind::Preview,
                    value,
                    body["request_id"].as_str().map(str::to_owned),
                ),
                Err(error) => {
                    self.reference_picker.status =
                        format!("Source unavailable · composer retained: {error}")
                }
            }
            return;
        }
        let result = result.and_then(|value| {
            match route {
                "/session/catalog" => {
                    self.reference_picker.catalog =
                        serde_json::from_value(value).map_err(invalid)?;
                    self.reference_picker.rows = self
                        .reference_picker
                        .catalog
                        .iter()
                        .map(|entry| Row {
                            detail_lines: vec![],
                            label: format!("{} · {}", entry.source.label, entry.source.branch),
                            detail: format!(
                                "{}\n{}",
                                entry.source.path,
                                entry
                                    .diagnostic
                                    .as_deref()
                                    .unwrap_or("Select to inspect branches")
                            ),
                            entry: None,
                            image: None,
                            tokens: 0,
                        })
                        .collect();
                    self.reference_picker.page = Page::Catalog;
                    self.reference_picker.filter_catalog();
                }
                "/session/branches" => {
                    self.reference_picker.branches =
                        serde_json::from_value(value).map_err(invalid)?;
                    self.reference_picker.rows = self
                        .reference_picker
                        .branches
                        .iter()
                        .map(|branch| Row {
                            detail_lines: vec![],
                            label: format!("{} · head {}", branch.name, branch.head),
                            detail: "Previewing a source branch does not switch the active session"
                                .into(),
                            entry: None,
                            image: None,
                            tokens: 0,
                        })
                        .collect();
                    self.reference_picker.page = Page::Branches;
                    if self.reference_picker.branches.is_empty() {
                        self.reference_request(
                            "/reference/preview",
                            json!({ "path": self.reference_picker.path }),
                        );
                    }
                }

                _ => {}
            }
            Ok(())
        });
        match result {
            Ok(()) => {
                self.reference_picker.sanitize_rows();
                if self.reference_picker.page != Page::Catalog {
                    self.reference_picker.selected = 0;
                }
                self.reference_picker.scroll = 0;
                self.reference_picker.status = "Preview is fixed at the source head; later source \
                                                changes cannot alter insertion"
                    .into();
            }
            Err(error) => {
                self.reference_picker.status =
                    format!("Source unavailable · composer retained: {error}")
            }
        }
    }
    pub(crate) fn reference_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.reference_picker.open = false;
            self.reference_picker.pending = None;
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
        if self.reference_picker.page == Page::Catalog {
            match key.code {
                KeyCode::Char('u')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    self.reference_picker.query.clear()
                }
                KeyCode::Char(c)
                    if !key.modifiers.intersects(
                        crossterm::event::KeyModifiers::CONTROL
                            | crossterm::event::KeyModifiers::ALT,
                    ) =>
                {
                    self.reference_picker.query.push(c)
                }
                KeyCode::Backspace => {
                    if let Some((index, _)) = self
                        .reference_picker
                        .query
                        .grapheme_indices(true)
                        .next_back()
                    {
                        self.reference_picker.query.truncate(index);
                    }
                }
                _ => {}
            }
            if matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace) {
                self.reference_picker.filter_catalog();
                return;
            }
        }
        if self.reference_picker.pending.is_some() || self.reference_picker.preparing {
            return;
        }
        match key.code {
            KeyCode::Up => {
                self.reference_picker.selected = self.reference_picker.selected.saturating_sub(1);
                self.reference_picker.scroll = 0;
            }
            KeyCode::Down => {
                self.reference_picker.selected = (self.reference_picker.selected + 1)
                    .min(self.reference_picker.row_count().saturating_sub(1));
                self.reference_picker.scroll = 0;
            }
            KeyCode::PageDown => {
                self.reference_picker.scroll = self.reference_picker.scroll.saturating_add(10)
            }
            KeyCode::PageUp => {
                self.reference_picker.scroll = self.reference_picker.scroll.saturating_sub(10)
            }
            KeyCode::Char('s') if self.reference_picker.page == Page::Preview => {
                self.reference_picker.show_system = !self.reference_picker.show_system;
                self.reference_picker.scroll = 0;
            }
            KeyCode::Char('n') => {
                self.open_references(self.reference_picker.replacement.clone(), false)
            }
            KeyCode::Char('x') if self.reference_picker.page == Page::Attached => {
                if self.reference_picker.selected < self.references.len() {
                    self.references.remove(self.reference_picker.selected);
                    self.changed();
                    self.queue_reference_work(WorkKind::Attached, Value::Null, None);
                }
            }
            KeyCode::Char(' ') if self.reference_picker.page == Page::Preview => {
                if let Some(row) = self
                    .reference_picker
                    .rows
                    .get(self.reference_picker.selected)
                {
                    if let Some(id) = &row.entry {
                        if !self.reference_picker.entries.remove(id) {
                            self.reference_picker.entries.insert(id.clone());
                        } else {
                            self.reference_picker
                                .images
                                .retain(|image| &image.entry_id != id);
                        }
                    } else if let Some(image) = &row.image {
                        if !self.reference_picker.entries.contains(&image.entry_id) {
                            self.reference_picker.status =
                                "Select the parent entry before including its image".into();
                        } else if !self.reference_picker.images.remove(image) {
                            self.reference_picker.images.insert(image.clone());
                        }
                    }
                }
                self.reference_picker.update_estimate();
            }
            KeyCode::Enter if self.reference_picker.page == Page::Catalog => {
                if let Some(entry) = self
                    .reference_picker
                    .catalog
                    .get(self.reference_picker.row_index())
                {
                    if let Some(error) = &entry.diagnostic {
                        self.reference_picker.status = error.clone();
                        return;
                    }
                    let path = entry.source.path.clone();
                    self.reference_picker.path = path.clone();
                    self.reference_request("/session/branches", json!({ "path": path }));
                }
            }
            KeyCode::Enter if self.reference_picker.page == Page::Branches => {
                if let Some(branch) = self
                    .reference_picker
                    .branches
                    .get(self.reference_picker.selected)
                {
                    self.reference_request(
                        "/reference/preview",
                        json!({ "path": self.reference_picker.path, "head": branch.head }),
                    );
                }
            }
            KeyCode::Char('i') if self.reference_picker.page == Page::Preview => {
                self.insert_reference()
            }
            _ => {}
        }
    }
    fn insert_reference(&mut self) {
        let Some(preview) = &self.reference_picker.preview else {
            return;
        };
        if self.reference_picker.entries.is_empty() {
            self.reference_picker.status = "Select at least one source entry".into();
            return;
        }
        let selection = Selection {
            entry_ids: Some(self.reference_picker.entries.iter().cloned().collect()),
            images: self.reference_picker.images.iter().cloned().collect(),
        };
        let _ = preview;
        self.queue_reference_work(
            WorkKind::Freeze,
            json!({ "id": self.id(), "selection": selection }),
            self.reference_picker.expected_source.clone(),
        );
    }
    fn accept_frozen_reference(&mut self, reference: Arc<reference::Reference>) {
        if let Some(replacement) = &self.reference_picker.replacement {
            if replacement.apply(&self.editor.text(), "").is_none() {
                self.reference_picker.status =
                    "Composer changed; cancel and choose the source again".into();
                return;
            }
            let _ = self
                .editor
                .replace_range(replacement.token.range.clone(), "");
        }
        self.references.push(reference);
        self.changed();
        self.reference_picker.open = false;
        self.notice =
            "Frozen session reference inserted · /references reviews or removes it".into();
    }
}
fn invalid(error: serde_json::Error) -> Fault {
    Fault::new("InvalidReference", "tui", error.to_string())
}
pub fn render(
    app: &mut App,
    buf: &mut Buffer,
    area: Rect,
    p: Palette,
) -> (Geometry, Option<(u16, u16)>) {
    let mut g = Geometry {
        modal: Some(area),
        ..Default::default()
    };
    let inner = Rect::new(
        area.x + 1,
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );
    app.reference_picker.width = usize::from(inner.width.max(1));
    let picker = &app.reference_picker;
    let title = match picker.page {
        Page::Catalog => "Reference · choose a saved session",
        Page::Branches => "Reference · choose a source branch",
        Page::Preview => "Reference · fixed source preview",
        Page::Attached => "Reference · attached draft snapshots",
    };
    Paragraph::new(title)
        .style(p.style().fg(p.heading).add_modifier(Modifier::BOLD))
        .render(Rect::new(inner.x, inner.y, inner.width, 1), buf);
    let source = picker
        .preview
        .as_ref()
        .map(|v| {
            format!(
                "{} / {} @ {:?} · source system {} · {}",
                v.source.label,
                v.source.branch,
                v.source.head,
                if v.source_system.is_some() {
                    "known"
                } else {
                    "unknown"
                },
                picker.comparison
            )
        })
        .unwrap_or_default();
    let source = if picker.page == Page::Catalog {
        format!(
            "Search: {} · {} of {} sessions",
            picker.query,
            picker.visible.len(),
            picker.catalog.len()
        )
    } else {
        source
    };
    let estimate = picker.selected_estimate;
    let budget = format!(
        "Selected ~{estimate} tokens · estimated available {} · no truncation{}",
        picker
            .available
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".into()),
        if picker.available.is_some_and(|n| estimate > n) {
            " · OVER BUDGET"
        } else if !picker.images.is_empty()
            && picker
                .budget
                .as_ref()
                .and_then(|s| s.model.as_ref())
                .is_some_and(|m| !m.capabilities.images)
        {
            " · MODEL DOES NOT SUPPORT IMAGES"
        } else {
            ""
        }
    );
    Paragraph::new(crate::text::clean(&format!("{source}\n{budget}")))
        .style(p.style().fg(p.muted))
        .render(Rect::new(inner.x, inner.y + 1, inner.width, 2), buf);
    let content = Rect::new(
        inner.x,
        inner.y + 3,
        inner.width,
        inner.height.saturating_sub(7),
    );
    let list_height = (content.height / 2).min(8);
    let start = picker
        .selected
        .saturating_sub(list_height.saturating_sub(1) as usize);
    for (row, index) in (start..picker.row_count())
        .take(list_height as usize)
        .enumerate()
    {
        let actual = if picker.page == Page::Catalog {
            picker.visible[index]
        } else {
            index
        };
        let choice = &picker.rows[actual];
        let checked = choice
            .entry
            .as_ref()
            .is_some_and(|id| picker.entries.contains(id))
            || choice
                .image
                .as_ref()
                .is_some_and(|image| picker.images.contains(image));
        let text = format!(
            "{} {}{}",
            if index == picker.selected { ">" } else { " " },
            if picker.page == Page::Preview {
                if checked { "[x] " } else { "[ ] " }
            } else {
                ""
            },
            choice.label
        );
        Paragraph::new(text)
            .style(if index == picker.selected {
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
    if let Some(row) = picker.rows.get(picker.row_index()) {
        let lines = if picker.show_system {
            &picker.system_lines
        } else {
            &row.detail_lines
        };
        for (y, line) in lines
            .iter()
            .skip(picker.scroll)
            .take(content.height.saturating_sub(list_height) as usize)
            .enumerate()
        {
            Paragraph::new(line.as_str()).style(p.style()).render(
                Rect::new(
                    content.x,
                    content.y + list_height + y as u16,
                    content.width,
                    1,
                ),
                buf,
            );
        }
    }
    Paragraph::new(crate::text::clean(&picker.status))
        .style(p.style().fg(p.warning))
        .wrap(Wrap { trim: false })
        .render(
            Rect::new(inner.x, inner.bottom().saturating_sub(4), inner.width, 2),
            buf,
        );
    Paragraph::new(
        "Type to search sessions · ↑↓ select · Enter open · Space include · i insert\nPgUp/Dn \
         read · s source system · x remove attached · n new source · Esc cancel",
    )
    .style(p.style().fg(p.muted))
    .wrap(Wrap { trim: false })
    .render(
        Rect::new(inner.x, inner.bottom().saturating_sub(2), inner.width, 2),
        buf,
    );
    (g, None)
}
pub fn mouse(app: &mut App, g: &Geometry, event: MouseEvent) -> bool {
    match event.kind {
        MouseEventKind::ScrollDown => {
            app.reference_picker.scroll = app.reference_picker.scroll.saturating_add(3)
        }
        MouseEventKind::ScrollUp => {
            app.reference_picker.scroll = app.reference_picker.scroll.saturating_sub(3)
        }
        MouseEventKind::Down(MouseButton::Left)
            if g.transcript.contains((event.column, event.row).into()) =>
        {
            let start = app
                .reference_picker
                .selected
                .saturating_sub(g.transcript.height.saturating_sub(1) as usize);
            app.reference_picker.selected = (start + (event.row - g.transcript.y) as usize)
                .min(app.reference_picker.row_count().saturating_sub(1));
            app.reference_picker.scroll = 0;
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    async fn settle(app: &mut App) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while app.reference_picker.preparing {
                app.poll_references();
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }
    pub(crate) fn preview() -> Preview {
        serde_json::from_value(json!({
            "source": {
                "session_id": 9,
                "label": "Source task",
                "path": "/removed/source.jsonl",
                "branch": "main",
                "head": 4,
            },
            "entries": [{
                "id": "record:4",
                "item": {
                    "type": "message",
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "original source" },
                        { "type": "image", "media_type": "image/png", "data": "fixed-image-bytes" }
                    ],
                },
            }],
            "source_system": null,
            "projection_version": 1,
            "estimated_tokens": 6,
        }))
        .unwrap()
    }
    pub(crate) fn frozen() -> reference::Reference {
        preview()
            .freeze("fixed".into(), &Selection::default())
            .unwrap()
    }
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    #[tokio::test]
    async fn inserted_reference_is_fixed_across_source_change_and_draft_recovery() {
        let mut app = crate::app::tests::app();
        app.editor.restore("Ask about this", 0).unwrap();
        app.reference_picker.open = true;
        app.reference_picker.preview_rows(preview());
        app.reference_key(key(KeyCode::Char('i')));
        settle(&mut app).await;
        Arc::make_mut(app.reference_picker.preview.as_mut().unwrap())
            .entries
            .clear();
        assert!(
            app.references[0]
                .content
                .iter()
                .any(|b| matches!(b,Block::Text{text} if text=="original source"))
        );
        assert!(
            !app.references[0]
                .content
                .iter()
                .any(|b| matches!(b, Block::Image { .. }))
        );
        let saved = crate::model::SavedDraft {
            version: 1,
            session: app.session.clone(),
            frontend: "reference-test".into(),
            sessions: Default::default(),
            draft: app.draft(),
            pending: None,
        };
        let recovered: crate::model::SavedDraft =
            serde_json::from_value(serde_json::to_value(saved).unwrap()).unwrap();
        app.references.clear();
        app.editor.restore("", 0).unwrap();
        app.recovery = Some(recovered);
        app.restore_recovery();
        assert_eq!(app.references[0].source.head, Some(4));
        assert_eq!(app.editor.text(), "Ask about this");
        assert!(
            app.references[0]
                .content
                .iter()
                .any(|b| matches!(b,Block::Text{text} if text=="original source"))
        );
    }
    #[tokio::test]
    async fn images_require_explicit_selection_and_follow_parent_entry_selection() {
        let mut app = crate::app::tests::app();
        app.reference_picker.open = true;
        app.reference_picker.preview_rows(preview());
        app.reference_picker.selected = 1;
        app.reference_key(key(KeyCode::Char(' ')));
        assert_eq!(app.reference_picker.images.len(), 1);
        app.reference_picker.selected = 0;
        app.reference_key(key(KeyCode::Char(' ')));
        assert!(app.reference_picker.images.is_empty());
        app.reference_key(key(KeyCode::Char(' ')));
        app.reference_picker.selected = 1;
        app.reference_key(key(KeyCode::Char(' ')));
        app.reference_key(key(KeyCode::Char('i')));
        settle(&mut app).await;
        assert_eq!(app.references[0].included_images[0].block_index, 1);
        assert!(
            app.references[0]
                .content
                .iter()
                .any(|b| matches!(b,Block::Image{data,..} if data=="fixed-image-bytes"))
        );
    }
    #[tokio::test]
    async fn cancel_session_completion_retains_composer_and_ignores_late_source_reply() {
        let mut app = crate::app::tests::app();
        app.editor.restore("Look @session", 13).unwrap();
        app.sync_completion();
        let index = app
            .autocomplete
            .items
            .iter()
            .position(|i| i.action == crate::autocomplete::Action::Session)
            .unwrap();
        app.accept_completion(index);
        let (_, id) = app.reference_picker.pending.clone().unwrap();
        app.key(key(KeyCode::Esc));
        app.reference_reply(
            "/session/catalog",
            &json!({ "request_id": id }),
            Ok(json!([])),
        );
        assert!(!app.reference_picker.open);
        assert_eq!(app.editor.text(), "Look @session");
        assert!(app.references.is_empty());
    }
    #[tokio::test]
    async fn deselection_changes_estimate_without_truncating_source_snapshot() {
        let mut app = crate::app::tests::app();
        app.reference_picker.open = true;
        app.reference_picker.preview_rows(preview());
        let full = app.reference_picker.selected_estimate;
        assert!(full > 0);
        app.reference_key(key(KeyCode::Char(' ')));
        assert_eq!(app.reference_picker.selected_estimate, 0);
        assert_eq!(
            app.reference_picker.preview.as_ref().unwrap().entries.len(),
            1
        );
        app.reference_picker.available = Some(1);
        let area = Rect::new(0, 0, 80, 24);
        render(
            &mut app,
            &mut Buffer::empty(area),
            area,
            Palette::new(false, true),
        );
    }
    #[tokio::test]
    async fn catalog_search_preserves_matching_selection_and_input_focus() {
        let mut app = crate::app::tests::app();
        app.reference_picker.open = true;
        app.reference_picker.catalog = serde_json::from_value(json!([
            {
                "source": {
                    "session_id": 1,
                    "label": "Notes Alpha",
                    "path": "/a.jsonl",
                    "branch": "main",
                    "head": 1,
                },
                "diagnostic": null,
            },
            {
                "source": {
                    "session_id": 2,
                    "label": "Notes Beta",
                    "path": "/b.jsonl",
                    "branch": "main",
                    "head": 2,
                },
                "diagnostic": null,
            }
        ]))
        .unwrap();
        app.reference_picker.filter_catalog();
        app.reference_picker.selected = 1;
        for character in "Notes".chars() {
            app.reference_key(key(KeyCode::Char(character)));
        }
        assert_eq!(app.reference_picker.row_index(), 1);
        app.reference_key(key(KeyCode::Char(' ')));
        app.reference_key(key(KeyCode::Char('B')));
        assert_eq!(app.reference_picker.visible, vec![1]);
        assert_eq!(app.reference_picker.row_index(), 1);
        app.reference_key(key(KeyCode::Backspace));
        assert_eq!(app.reference_picker.row_index(), 1);
        assert_eq!(app.editor.text(), "");
        assert!(app.reference_picker.open);
    }
    #[tokio::test]
    async fn preview_assembly_runs_off_the_ui_thread_and_stale_prepared_sources_are_ignored() {
        let mut app = crate::app::tests::app();
        app.reference_picker.open = true;
        app.reference_picker.width = 80;
        app.reference_picker.expected_source = Some("current".into());
        let mut source = preview();
        let instructions = Item::Message {
            role: "system".into(),
            content: vec![Block::Text {
                text: "same captured instruction\n".repeat(10000),
            }],
        };
        source.source_system = Some(vec![instructions.clone()]);
        let mut model = eden_protocol::models::ModelTarget::default();
        model.limits.context_window = 1_000_000;
        let document =
            json!({ "entries": [{ "id": "system", "item": instructions }], "tools": [] });
        let budget = json!({
            "revision": { "session_id": 7, "sequence": 1, "head": 1, "branch": "main" },
            "original": document,
            "effective": document,
            "edits": [],
            "last_request": null,
            "model": model,
            "budget": { "reserve_tokens": { "tokens": 1000 } },
        });
        app.queue_reference_work(WorkKind::Budget, budget, None);
        source.entries[0].item = Item::Message {
            role: "user".into(),
            content: vec![Block::Text {
                text: "source text\n".repeat(10000),
            }],
        };
        app.queue_reference_work(
            WorkKind::Preview,
            serde_json::to_value(&source).unwrap(),
            Some("current".into()),
        );
        assert!(app.reference_picker.preparing);
        assert!(app.reference_picker.rows.is_empty());
        settle(&mut app).await;
        assert_ne!(
            app.reference_picker.worker_thread.unwrap(),
            std::thread::current().id()
        );
        assert_eq!(
            app.reference_picker.preview.as_ref().unwrap().source,
            source.source
        );
        assert!(!app.reference_picker.rows[0].detail_lines.is_empty());
        assert!(
            app.reference_picker
                .comparison
                .starts_with("same as inspected")
        );
        assert!(app.reference_picker.available.is_some());
        let (tx, _rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        app.reference_picker.worker = Some((tx, reply_rx));
        let stale = prepare_work(
            &mut Picker::default(),
            Work {
                kind: WorkKind::Preview,
                value: serde_json::to_value(preview()).unwrap(),
                source: Some("old source".into()),
                generation: 999,
                composer: String::new(),
                references: vec![],
                preview: None,
                width: 80,
            },
        );
        reply_tx.send(stale).unwrap();
        assert!(!app.poll_references());
        assert!(app.reference_picker.rows[0].detail.len() > 10000);
        app.reference_picker.open = false;
        let late = prepare_work(
            &mut Picker::default(),
            Work {
                kind: WorkKind::Preview,
                value: serde_json::to_value(preview()).unwrap(),
                source: Some("current".into()),
                generation: 1000,
                composer: String::new(),
                references: vec![],
                preview: None,
                width: 80,
            },
        );
        reply_tx.send(late).unwrap();
        assert!(!app.poll_references());
        assert!(!app.reference_picker.open);
    }
}
