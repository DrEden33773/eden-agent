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
use std::collections::BTreeSet;
#[derive(Default, PartialEq)]
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
    preview: Option<Preview>,
    rows: Vec<Row>,
    selected: usize,
    scroll: usize,
    entries: BTreeSet<String>,
    images: BTreeSet<ImageSelection>,
    pending: Option<(String, String)>,
    budget_request: Option<String>,
    budget: Option<eden_protocol::context_edit::Snapshot>,
    available: Option<u64>,
    selected_estimate: u64,
    system_detail: String,
    comparison: String,
    show_system: bool,
    replacement: Option<Replacement>,
    status: String,
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
    fn sanitize_rows(&mut self) {
        for row in &mut self.rows {
            row.label = crate::text::clean(&row.label);
            row.detail = crate::text::clean(&row.detail);
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
        self.sanitize_rows();
        self.preview = Some(preview);
        self.page = Page::Preview;
        self.update_estimate();
        self.update_comparison();
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
    pub(crate) fn open_references(&mut self, replacement: Option<Replacement>, attached: bool) {
        self.reference_picker = Picker {
            open: true,
            replacement,
            ..Default::default()
        };
        self.autocomplete.dismiss();
        self.clipboard_generation += 1;
        if attached {
            self.reference_picker.page = Page::Attached;
            self.reference_picker.attached_rows(&self.references);
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
                self.reference_picker.budget = serde_json::from_value(value).ok();
                self.reference_picker
                    .recalculate_budget(&self.editor.text(), &self.references);
                self.reference_picker.update_comparison();
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
                }
                "/session/branches" => {
                    self.reference_picker.branches =
                        serde_json::from_value(value).map_err(invalid)?;
                    self.reference_picker.rows = self
                        .reference_picker
                        .branches
                        .iter()
                        .map(|branch| Row {
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
                "/reference/preview" => {
                    self.reference_picker
                        .preview_rows(serde_json::from_value(value).map_err(invalid)?);
                }
                _ => {}
            }
            Ok(())
        });
        match result {
            Ok(()) => {
                self.reference_picker.sanitize_rows();
                self.reference_picker.selected = 0;
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
        if self.reference_picker.pending.is_some() {
            return;
        }
        match key.code {
            KeyCode::Up => {
                self.reference_picker.selected = self.reference_picker.selected.saturating_sub(1);
                self.reference_picker.scroll = 0;
            }
            KeyCode::Down => {
                self.reference_picker.selected = (self.reference_picker.selected + 1)
                    .min(self.reference_picker.rows.len().saturating_sub(1));
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
                    self.reference_picker.attached_rows(&self.references);
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
                    .get(self.reference_picker.selected)
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
        match preview.freeze(self.id(), &selection) {
            Ok(reference) => {
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
                self.references.push(reference.into());
                self.changed();
                self.reference_picker.open = false;
                self.notice =
                    "Frozen session reference inserted · /references reviews or removes it".into();
            }
            Err(error) => self.reference_picker.status = error.to_string(),
        }
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
    let picker = &app.reference_picker;
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
    for (row, (index, choice)) in picker
        .rows
        .iter()
        .enumerate()
        .skip(start)
        .take(list_height as usize)
        .enumerate()
    {
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
    if let Some(row) = picker.rows.get(picker.selected) {
        Paragraph::new(if picker.show_system {
            picker.system_detail.as_str()
        } else {
            row.detail.as_str()
        })
        .style(p.style())
        .wrap(Wrap { trim: false })
        .scroll((picker.scroll.min(u16::MAX as usize) as u16, 0))
        .render(
            Rect::new(
                content.x,
                content.y + list_height,
                content.width,
                content.height.saturating_sub(list_height),
            ),
            buf,
        );
    }
    Paragraph::new(crate::text::clean(&picker.status))
        .style(p.style().fg(p.warning))
        .wrap(Wrap { trim: false })
        .render(
            Rect::new(inner.x, inner.bottom().saturating_sub(4), inner.width, 2),
            buf,
        );
    Paragraph::new(
        "↑↓ select · Enter open · Space include entry/image · i insert frozen quote\nPgUp/Dn read \
         · s source system · x remove attached · n new source · Esc cancel",
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
                .min(app.reference_picker.rows.len().saturating_sub(1));
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
        app.reference_picker
            .preview
            .as_mut()
            .unwrap()
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
}
