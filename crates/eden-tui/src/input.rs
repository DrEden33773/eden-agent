use crate::{
    app::App,
    model::{Dialog, Focus},
    selection::Selection,
    text,
    view::{Geometry, HitAction},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::time::Instant;
#[derive(Default)]
enum Drag {
    #[default]
    None,
    Transcript,
    Editor,
    Field,
    Scrollbar,
}
#[derive(Default)]
pub struct Pointer {
    drag: Drag,
    last: Option<(Instant, u16, u16)>,
    clicks: u8,
}
fn activate(app: &mut App, action: &HitAction, double: bool) {
    match action {
        HitAction::Close => app.dialog_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        HitAction::Choice(index) => {
            if let Some(Dialog::Palette { selected, .. }) = &mut app.dialog {
                *selected = *index;
            }
            if double {
                app.dialog_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            }
        }
        HitAction::Setting(index) => {
            if let Some(Dialog::Settings { selected }) = &mut app.dialog {
                *selected = *index;
            }
            app.dialog_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        }
        HitAction::SettingTab(index) => {
            if let Some(Dialog::Settings { selected }) = &mut app.dialog {
                *selected = index * 3;
            }
        }
        HitAction::Field(index) => app.form_click(*index),
        HitAction::FieldChoice(index) => app.form_choice(*index),
        HitAction::Completion(index) => app.accept_completion(*index),
        HitAction::Apply => {
            app.dialog_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        }
    }
}
fn scrollbar(app: &mut App, g: &Geometry, y: u16) {
    let r = g.transcript;
    let maximum = app.rows.len().saturating_sub(r.height as usize);
    let row = y.saturating_sub(r.y).min(r.height.saturating_sub(1)) as usize;
    let top = row * maximum / r.height.saturating_sub(1).max(1) as usize;
    app.follow = top == maximum;
    app.top = top;
    if let Some(row) = app.rows.get(top) {
        app.anchor = row.anchor;
    }
    if app.follow {
        app.unseen = 0;
    }
}
pub fn mouse(app: &mut App, g: &Geometry, m: MouseEvent, p: &mut Pointer) -> bool {
    let (x, y) = (m.column, m.row);
    if m.kind == MouseEventKind::Moved {
        let changed = app.hover != Some((x, y));
        app.hover = Some((x, y));
        return changed;
    }
    if m.kind == MouseEventKind::Down(MouseButton::Left) {
        p.clicks = if p
            .last
            .is_some_and(|(t, a, b)| t.elapsed().as_millis() < 400 && (a, b) == (x, y))
        {
            p.clicks.saturating_add(1)
        } else {
            1
        };
        p.last = Some((Instant::now(), x, y));
        p.drag = Drag::None;
    }
    if app.dialog.is_some() {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = g.hits.iter().rev().find(|h| h.area.contains((x, y).into())) {
                    activate(app, &hit.action, p.clicks == 2);
                } else if let Some(r) = g.field_editor.filter(|r| r.contains((x, y).into())) {
                    let _ = app.field_editor.mouse(
                        x - r.x,
                        y - r.y,
                        m.modifiers.contains(KeyModifiers::SHIFT),
                    );
                    p.drag = Drag::Field;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if matches!(p.drag, Drag::Field) => {
                if let Some(r) = g.field_editor {
                    let _ = app.field_editor.mouse(
                        x.saturating_sub(r.x).min(r.width.saturating_sub(1)),
                        y.saturating_sub(r.y).min(r.height.saturating_sub(1)),
                        true,
                    );
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if g.modal.is_some_and(|r| r.contains((x, y).into())) =>
            {
                let up = m.kind == MouseEventKind::ScrollUp;
                if matches!(
                    app.dialog,
                    Some(Dialog::Palette { .. } | Dialog::Settings { .. } | Dialog::Form { .. })
                ) {
                    for _ in 0..3 {
                        app.dialog_key(KeyEvent::new(
                            if up { KeyCode::Up } else { KeyCode::Down },
                            KeyModifiers::NONE,
                        ));
                    }
                } else {
                    app.dialog_scroll =
                        app.dialog_scroll
                            .saturating_add_signed(if up { -3 } else { 3 });
                }
            }
            MouseEventKind::Up(MouseButton::Left) => p.drag = Drag::None,
            _ => {}
        }
        return true;
    }
    match m.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let amount = if m.modifiers.contains(KeyModifiers::ALT) {
                15
            } else {
                3
            };
            let delta = if m.kind == MouseEventKind::ScrollUp {
                -amount
            } else {
                amount
            };
            if g.completion.is_some_and(|r| r.contains((x, y).into())) {
                app.autocomplete.move_selection(delta);
            } else if g.inspector.is_some_and(|r| r.contains((x, y).into())) {
                app.inspector_scroll = app.inspector_scroll.saturating_add_signed(delta);
            } else if g.transcript.contains((x, y).into()) {
                app.scroll(delta);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(hit) = g.hits.iter().rev().find(|h| h.area.contains((x, y).into())) {
                activate(app, &hit.action, p.clicks == 2);
                return true;
            }
            if g.latest.is_some_and(|r| r.contains((x, y).into())) {
                app.bottom();
                return true;
            }
            if let Some(r) = g.navigator.filter(|r| r.contains((x, y).into())) {
                let start = app
                    .selected
                    .saturating_sub(r.height.saturating_sub(3) as usize);
                let i = start + y.saturating_sub(r.y + 2) as usize;
                if let Some(message) = app.messages.get(i) {
                    app.selected = i;
                    app.follow = false;
                    app.anchor = crate::model::Anchor {
                        record: message.id,
                        line: 0,
                        byte: 0,
                    };
                }
            } else if g.composer.contains((x, y).into()) {
                app.focus = Focus::Editor;
                app.selection = None;
                let _ = app.editor.mouse(
                    x - g.composer.x,
                    y - g.composer.y,
                    m.modifiers.contains(KeyModifiers::SHIFT),
                );
                p.drag = Drag::Editor;
            } else if g.transcript.contains((x, y).into()) {
                if x == g.transcript.right() - 1 && app.rows.len() > g.transcript.height as usize {
                    p.drag = Drag::Scrollbar;
                    scrollbar(app, g, y);
                    return true;
                }
                app.focus = Focus::Transcript;
                app.follow = false;
                p.drag = Drag::Transcript;
                if let Some(row) = app.rows.get(app.top + (y - g.transcript.y) as usize) {
                    let point = row.anchor;
                    let col = (x - g.transcript.x).saturating_sub(row.inset) as usize;
                    app.selection = Some(Selection::from_row(row, col));
                    if let Some(i) = app.messages.iter().position(|m| m.id == point.record) {
                        app.selected = i;
                    }
                }
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => match p.drag {
            Drag::Scrollbar => scrollbar(app, g, y),
            Drag::Editor => {
                let r = g.composer;
                let _ = app.editor.mouse(
                    x.saturating_sub(r.x).min(r.width.saturating_sub(1)),
                    y.saturating_sub(r.y).min(r.height.saturating_sub(1)),
                    true,
                );
            }
            Drag::Transcript => {
                if let Some(mut selection) = app.selection {
                    if y < g.transcript.y {
                        app.scroll(-1);
                    } else if y >= g.transcript.bottom() {
                        app.scroll(1);
                    }
                    let i = app.top
                        + y.saturating_sub(g.transcript.y)
                            .min(g.transcript.height.saturating_sub(1))
                            as usize;
                    if let Some(row) = app.rows.get(i) {
                        selection.extend(
                            row,
                            x.saturating_sub(g.transcript.x).saturating_sub(row.inset) as usize,
                        );
                        app.selection = Some(selection);
                    }
                }
            }
            _ => {}
        },
        MouseEventKind::Up(MouseButton::Left) => {
            if matches!(p.drag, Drag::Transcript)
                && let Some(selection) = app.selection
                && selection.is_empty()
            {
                let a = selection.origin();
                if p.clicks >= 3 {
                    app.selection = Selection::line(&app.rows, a);
                } else if p.clicks == 2 {
                    app.selection = Some(Selection::word(&app.rows, a));
                } else {
                    app.selection = None;
                    if a.line == 0
                        || app.rows.get(text::locate(&app.rows, a)).is_some_and(|r| {
                            r.text.contains("show more")
                                || r.text.contains("Collapse")
                                || r.text.contains("Inspect full output")
                        })
                    {
                        if app
                            .rows
                            .get(text::locate(&app.rows, a))
                            .is_some_and(|r| r.text.contains("Inspect full output"))
                        {
                            app.preferences.inspector = true;
                            app.inspector_scroll = 0;
                        } else {
                            app.toggle_message();
                        }
                    }
                }
            }
            p.drag = Drag::None;
        }
        _ => {}
    }
    true
}
