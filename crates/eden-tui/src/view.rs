use crate::{app::App, model::*, text};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap},
};

use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color,
    pub panel: Color,
    pub fg: Color,
    pub muted: Color,
    pub accent: Color,
    pub success: Color,
    pub error: Color,
    pub border: Color,
    pub selected: Color,
    pub heading: Color,
    pub key: Color,
    pub code: Color,
    pub link: Color,
    pub quote: Color,
    pub number: Color,
    pub type_color: Color,
    pub function: Color,
    pub dim: Color,
    pub warning: Color,
}
impl Palette {
    pub fn new(light: bool, mono: bool) -> Self {
        // GrokNight foreground hierarchy; theme extensions still own semantic overrides.
        let color = |dark: u32, light_rgb: u32| {
            if mono {
                Color::Reset
            } else {
                let n = if light { light_rgb } else { dark };
                Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8)
            }
        };
        Self {
            bg: Color::Reset,
            panel: Color::Reset,
            selected: Color::Reset,
            fg: color(0xe1e1e1, 0x25232b),
            muted: color(0xc8c8c8, 0x686473),
            accent: color(0xbb9af7, 0x673699),
            success: color(0x9ece6a, 0x167846),
            error: color(0xf7768e, 0xb42c40),
            border: color(0x505058, 0x8a8595),
            heading: color(0x7aa2f7, 0x4f4b99),
            key: color(0x73daca, 0x086b69),
            code: color(0x9ece6a, 0x276d38),
            link: color(0x7aa2f7, 0x2a5ba7),
            quote: color(0xbb9af7, 0x884d76),
            number: color(0xff9e64, 0x915514),
            type_color: color(0xe0af68, 0x8b6811),
            function: color(0x7aa2f7, 0x2a5ba7),
            dim: color(0x787878, 0x787280),
            warning: color(0xe0af68, 0x8b6811),
        }
    }
    pub fn style(self) -> Style {
        Style::default().fg(self.fg).bg(self.bg)
    }
}
#[derive(Clone, Debug, Default)]
pub struct Geometry {
    pub image: Option<crate::image_preview::Placement>,
    pub hits: Vec<Hit>,
    pub field_editor: Option<Rect>,
    pub latest: Option<Rect>,
    pub completion: Option<Rect>,
    pub transcript: Rect,
    pub composer: Rect,
    pub inspector: Option<Rect>,
    pub navigator: Option<Rect>,
    pub modal: Option<Rect>,
}
#[derive(Clone, Debug)]
pub struct Hit {
    pub area: Rect,
    pub action: HitAction,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HitAction {
    Completion(usize),
    FieldChoice(usize),
    Choice(usize),
    Setting(usize),
    Field(usize),
    Apply,
    Close,
    SettingTab(usize),
}
fn hit(g: &mut Geometry, area: Rect, action: HitAction) {
    if area.width > 0 && area.height > 0 {
        g.hits.push(Hit { area, action });
    }
}
fn line(buf: &mut Buffer, area: Rect, spans: Vec<Span<'_>>) {
    if area.width > 0 && area.height > 0 {
        buf.set_line(
            area.x,
            area.y,
            &crate::grok_render::fit_line_to_width(Line::from(spans), usize::from(area.width)),
            area.width,
        );
    }
}
fn inset(r: Rect, x: u16, y: u16) -> Rect {
    Rect::new(
        r.x + x.min(r.width),
        r.y + y.min(r.height),
        r.width.saturating_sub(x * 2),
        r.height.saturating_sub(y * 2),
    )
}
fn label(buf: &mut Buffer, r: Rect, text: &str, style: Style) {
    if r.width > 0 && r.height > 0 {
        buf.set_stringn(r.x, r.y, text::clean(text), r.width as usize, style);
    }
}
fn row_rect(r: Rect, row: u16) -> Rect {
    Rect::new(r.x, r.y + row, r.width, 1)
}
fn panel(buf: &mut Buffer, r: Rect, title: &str, p: Palette) {
    Clear.render(r, buf);
    Block::default()
        .title(Line::from(vec![Span::styled(
            format!(" {title} "),
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        )]))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(p.border))
        .style(Style::default().bg(p.panel))
        .render(r, buf);
}
fn dock_panel(buf: &mut Buffer, r: Rect, title: &str, p: Palette) {
    Clear.render(r, buf);
    Block::default()
        .title(Line::styled(
            format!(" {title} "),
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(p.border))
        .style(p.style())
        .render(r, buf);
}
fn close_button(r: Rect) -> Rect {
    let width = r.width.saturating_sub(2).min(3);
    Rect::new(
        r.right().saturating_sub(width + 1).max(r.x),
        r.y,
        width,
        r.height.min(1),
    )
}

fn center(area: Rect, width: u16, height: u16) -> Rect {
    let w = width
        .min(area.width.saturating_sub(8))
        .max(area.width.min(1));
    let h = height
        .min(area.height.saturating_sub(2))
        .max(area.height.min(1));
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

pub fn render(
    app: &mut App,
    buf: &mut Buffer,
    area: Rect,
    mono: bool,
) -> (Geometry, Option<(u16, u16)>) {
    let mut p = Palette::new(app.preferences.light, mono);
    if let Some(theme) = &app.theme {
        let resolve = |token| {
            let rgb = theme.resolve(!app.preferences.light, token);
            if mono || rgb == eden_ui_sdk::COLOR_DEFAULT {
                Color::Reset
            } else {
                Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
            }
        };
        p.fg = resolve(0);
        p.muted = resolve(1);
        p.accent = resolve(2);
        p.bg = resolve(3);
        p.success = resolve(4);
        p.error = resolve(5);
        p.border = resolve(6);
        p.panel = resolve(7);
        p.selected = resolve(8);
        p.heading = resolve(9);
        p.key = resolve(10);
        p.code = resolve(11);
        p.link = resolve(12);
        p.quote = resolve(13);
        p.number = resolve(14);
        p.type_color = resolve(15);
        p.function = resolve(16);
        p.dim = resolve(17);
        p.warning = resolve(18);
    }
    for (name, rgb) in &app.preferences.colors {
        let color = if mono {
            Color::Reset
        } else {
            Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, *rgb as u8)
        };
        match name.as_str() {
            "foreground" => p.fg = color,
            "muted" => p.muted = color,
            "accent" => p.accent = color,
            "success" => p.success = color,
            "error" => p.error = color,
            "border" => p.border = color,
            "heading" => p.heading = color,
            "key" => p.key = color,
            "code" => p.code = color,
            "link" => p.link = color,
            "quote" => p.quote = color,
            "number" => p.number = color,
            "type" => p.type_color = color,
            "function" => p.function = color,
            "dim" => p.dim = color,
            "warning" => p.warning = color,
            _ => {}
        }
    }
    buf.set_style(area, p.style());
    let mut g = Geometry::default();
    if area.width < 8 || area.height < 5 {
        label(buf, area, "Resize · /quit", p.style());
        return (g, None);
    }
    if app.reference_picker.open {
        return crate::references::render(app, buf, area, p);
    }
    if app.context.open {
        return crate::context::render(app, buf, area, p, mono);
    }
    let tiny = area.height < 16;
    let pad = if area.width >= 90 { 2 } else { 1 };
    let content_width = area.width.saturating_sub(pad * 2);
    let editor_text = app.editor.text();
    let wrap_width = content_width.saturating_sub(4).max(1) as usize;
    let editor_lines = editor_text
        .split('\n')
        .map(|s| text::wrap(s, wrap_width).len())
        .sum::<usize>()
        .clamp(1, if tiny { 2 } else { 5 }) as u16;
    let footer = if app.preferences.footer && !tiny {
        2
    } else {
        0
    };
    let composer_height = (editor_lines + 2).min(area.height.saturating_sub(2));
    let status_height = if tiny && app.autocomplete.open { 0 } else { 1 };
    let top_padding = if tiny { 0 } else { 1 };
    let completion_height = if app.autocomplete.open && app.dialog.is_none() {
        (app.autocomplete.items.len().clamp(1, 6) as u16 + 1).min(
            area.height
                .saturating_sub(composer_height + footer + status_height + top_padding),
        )
    } else {
        0
    };
    let dock_request = dock_request(app);
    let docked = dock_request.is_some();
    let dock_height = dock_request.map_or(0, |wanted| bottom_panel(area, footer, wanted).height);
    let body_height = area.height.saturating_sub(
        footer
            + if docked {
                dock_height
            } else {
                composer_height + completion_height
            }
            + status_height
            + top_padding,
    );
    let body = Rect::new(
        area.x + pad,
        area.y + top_padding,
        content_width,
        body_height,
    );
    if top_padding > 0 {
        line(
            buf,
            Rect::new(area.x + pad, area.y, content_width, 1),
            vec![
                Span::styled(
                    "eden",
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  /  {}", app.session), Style::default().fg(p.dim)),
            ],
        );
    }
    let mut chat = body;
    if app.preferences.navigator && area.width >= 100 {
        let nav = Rect::new(chat.x, chat.y, 23, chat.height);
        g.navigator = Some(nav);
        chat.x += 25;
        chat.width = chat.width.saturating_sub(25);
        draw_nav(app, buf, nav, p);
    }
    if app.preferences.inspector {
        if chat.width >= 108 {
            let w = (chat.width * 45 / 100).max(44);
            g.inspector = Some(Rect::new(chat.right() - w, chat.y, w, chat.height));
            chat.width = chat.width.saturating_sub(w + 2);
        } else {
            g.inspector = Some(chat);
            chat.width = 0;
        }
    }
    g.transcript = chat;
    app.viewport = chat.height.max(1) as usize;
    if let Some(renderer) = &mut app.renderer {
        let document = serde_json::json!({
            "messages": app.messages,
            "preferences": app.preferences,
            "session": app.session,
            "scroll": app.top,
            "theme": {
                "dark": !app.preferences.light,
                "monochrome": mono,
                "foreground": rgb(p.fg),
                "muted": rgb(p.muted),
                "accent": rgb(p.accent),
                "background": rgb(p.bg),
                "success": rgb(p.success),
                "error": rgb(p.error),
            },
        });
        if let Err(error) = renderer.render(&document, buf, chat, mono) {
            app.notice = error.to_string();
            draw_transcript(app, buf, chat, p, mono);
        }
    } else {
        draw_transcript(app, buf, chat, p, mono);
    }
    if let Some(r) = g.inspector {
        draw_inspector(app, buf, r, p, mono);
    }
    let status = Rect::new(area.x + pad, body.bottom(), content_width, status_height);
    let active = matches!(
        app.phase,
        Phase::Running | Phase::Waiting | Phase::Cancelling
    );
    if active {
        let spinner = if app.preferences.motion {
            ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][app.animation_frame as usize % 10]
        } else {
            "●"
        };
        line(
            buf,
            status,
            vec![
                Span::styled(format!("{spinner} "), Style::default().fg(p.accent)),
                Span::styled(format!("{}…", app.phase.label()), Style::default().fg(p.fg)),
                Span::styled(
                    format!(
                        "  {}s · Ctrl+C interrupt",
                        app.run_started.elapsed().as_secs()
                    ),
                    Style::default().fg(p.muted),
                ),
            ],
        );
    } else if app.recovery.is_some() {
        label(
            buf,
            status,
            "Draft available  /recover",
            Style::default().fg(p.accent),
        );
    } else if !app.queue.is_empty() {
        label(
            buf,
            status,
            &format!("{} queued  /queue to review", app.queue.len()),
            Style::default().fg(p.accent),
        );
    } else if !app.attachments.is_empty() {
        label(
            buf,
            status,
            &format!(
                "Attached  {}",
                app.attachments
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join("  ")
            ),
            Style::default().fg(p.muted),
        );
    } else if !app.notice.is_empty() {
        label(
            buf,
            status,
            &app.notice,
            Style::default().fg(if app.phase == Phase::Failed {
                p.error
            } else {
                p.muted
            }),
        );
    }
    if app.dialog.is_none()
        && status_height > 0
        && app.top < app.document_height.saturating_sub(app.viewport)
        && chat.width > 0
    {
        let full = if app.unseen > 0 {
            format!("[ ↓ Back to bottom · {} updates · Ctrl+End ]", app.unseen)
        } else {
            "[ ↓ Back to bottom · Ctrl+End ]".into()
        };
        let hint = if full.width() <= status.width as usize {
            full
        } else if status.width >= 32 {
            "[ ↓ Back to bottom · Ctrl+End ]".into()
        } else {
            "[ ↓ Back to bottom ]".into()
        };
        let w = (hint.width() as u16).min(status.width);
        let r = Rect::new(status.x + (status.width - w) / 2, status.y, w, 1);
        Clear.render(status, buf);
        buf.set_style(status, p.style());
        let hovered = app.hover.is_some_and(|point| r.contains(point.into()));
        label(
            buf,
            r,
            &hint,
            Style::default().fg(if hovered { p.fg } else { p.accent }),
        );
        g.latest = Some(r);
    }
    let editor_outer = Rect::new(
        area.x + pad,
        status.bottom(),
        content_width,
        composer_height,
    );
    Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(
            Style::default().fg(if app.focus == Focus::Editor && app.dialog.is_none() {
                p.accent
            } else {
                p.border
            }),
        )
        .render(editor_outer, buf);
    label(
        buf,
        Rect::new(editor_outer.x, editor_outer.y + 1, 2, 1),
        "›",
        Style::default().fg(p.accent),
    );
    g.composer = Rect::new(
        editor_outer.x + 2,
        editor_outer.y + 1,
        editor_outer.width.saturating_sub(3),
        editor_lines,
    );
    let mut cursor = app
        .editor
        .draw(buf, g.composer, 0, !app.preferences.light, mono);
    if editor_text.is_empty() {
        label(
            buf,
            g.composer,
            if app.read_only {
                "Read only · / for commands"
            } else {
                "Ask anything…  / commands  @ files/session"
            },
            Style::default().fg(p.muted),
        );
    }
    if app.focus != Focus::Editor {
        cursor = None;
    }
    if footer > 0 {
        let first = Rect::new(area.x + pad, area.bottom() - footer, content_width, 1);
        let dim = Style::default().fg(p.muted);
        line(
            buf,
            first,
            vec![
                Span::styled(app.model.clone(), Style::default().fg(p.function)),
                Span::styled(
                    format!(
                        "  ·  {} · {} refs",
                        crate::projection::usage(&app.snapshot.history),
                        app.references.len()
                    ),
                    dim,
                ),
            ],
        );
        let second = row_rect(first, 1);
        line(
            buf,
            second,
            vec![
                Span::styled("workspace", Style::default().fg(p.function)),
                Span::styled(format!(" · {}", app.session), dim),
                Span::styled(app.connection.clone(), dim),
            ],
        );
        let help = "F1 help  F2 commands";
        if second.width > 75 {
            label(
                buf,
                Rect::new(
                    second.right() - help.len() as u16,
                    second.y,
                    help.len() as u16,
                    1,
                ),
                help,
                dim,
            );
        }
    }
    if completion_height > 0 {
        let r = Rect::new(
            area.x + pad,
            editor_outer.bottom(),
            content_width,
            completion_height,
        );
        draw_completion(app, buf, r, p, &mut g);
        g.completion = Some(r);
    }
    if app.dialog.is_some() {
        let backdrop = if docked {
            bottom_panel(area, footer, dock_request.unwrap_or(12))
        } else {
            Rect::new(
                area.x,
                area.y,
                area.width,
                area.height.saturating_sub(footer),
            )
        };
        Clear.render(backdrop, buf);
        buf.set_style(backdrop, p.style());
        let (r, c) = draw_dialog(app, buf, area, p, mono, &mut g);
        g.modal = Some(r);
        cursor = c;
    }
    (g, cursor)
}
fn draw_nav(app: &App, buf: &mut Buffer, area: Rect, p: Palette) {
    label(
        buf,
        area,
        "Turns",
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
    );
    let start = app
        .selected
        .saturating_sub(area.height.saturating_sub(3) as usize);
    for (i, m) in app
        .messages
        .iter()
        .enumerate()
        .skip(start)
        .take(area.height.saturating_sub(2) as usize)
    {
        let text = format!(
            "{} {}",
            if i == app.selected { "›" } else { " " },
            if m.summary.is_some() {
                "Run summary"
            } else {
                &m.title
            }
        );
        label(
            buf,
            row_rect(area, (i - start + 2) as u16),
            &text,
            Style::default().fg(if i == app.selected { p.fg } else { p.muted }),
        );
    }
}
fn draw_transcript(app: &mut App, buf: &mut Buffer, area: Rect, p: Palette, mono: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = area.width.saturating_sub(1).max(1) as usize;
    let key_for = |m: &Message| {
        (
            m.id,
            width,
            m.revision,
            m.expanded,
            app.preferences.compact,
            app.preferences.thinking,
            app.preferences.diff_split,
            app.preferences.basic,
            app.transcript_images.revision(m.id),
        )
    };
    let common = if app.cache.is_empty() {
        0
    } else {
        app.row_keys
            .iter()
            .copied()
            .zip(app.messages.iter().map(key_for))
            .take_while(|(a, b)| a == b)
            .count()
    };
    if common != app.messages.len() || common != app.row_keys.len() {
        let checkpoint = app
            .selection
            .filter(|selection| {
                app.row_keys
                    .iter()
                    .skip(common)
                    .any(|key| selection.contains_record(key.0))
            })
            .map(|selection| selection.checkpoint(&app.rows));
        let first_row = app
            .row_starts
            .get(common)
            .copied()
            .unwrap_or(app.rows.len());
        app.rows.truncate(first_row);
        for (old, m) in app
            .row_keys
            .iter()
            .skip(common)
            .zip(app.messages.iter().skip(common))
        {
            if old.0 == m.id && old.2 != m.revision {
                app.cache.remove(old);
            }
        }
        app.row_keys.truncate(common);
        app.row_starts.truncate(common);
        for m in app.messages.iter().skip(common) {
            let key = key_for(m);
            app.row_keys.push(key);
            app.row_starts.push(app.rows.len());
            let value = app.cache.entry(key).or_insert_with(|| {
                let mut rows = text::message_rows(m, width, &app.preferences);
                app.transcript_images.append_rows(m, width, &mut rows);
                rows
            });
            app.rows.extend_from_slice(value);
        }
        if checkpoint.is_some_and(|checkpoint| !checkpoint.matches(&app.rows)) {
            app.selection = None;
            app.notice = "Selection cleared · the selected content changed".into();
        }
        if app.cache.len() > app.messages.len() * 8 + 32 {
            app.cache.clear();
        }
    }
    app.document_height = app.rows.len();
    if app.rows.is_empty() {
        label(
            buf,
            row_rect(area, area.height / 3),
            "Eden",
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        );
        if area.height > 4 {
            label(
                buf,
                row_rect(area, area.height / 3 + 2),
                "Enter a request, or /help for keyboard shortcuts",
                Style::default().fg(p.muted),
            );
        }
        return;
    }
    app.top = if app.follow {
        app.rows.len().saturating_sub(area.height as usize)
    } else {
        text::locate(&app.rows, app.anchor).min(app.rows.len().saturating_sub(area.height as usize))
    };
    app.anchor = app.rows[app.top].anchor;
    let selected = app.messages.get(app.selected).map(|m| m.id);
    for (i, row) in app
        .rows
        .iter()
        .skip(app.top)
        .take(area.height as usize)
        .enumerate()
    {
        if let Some(image) = &row.image {
            crate::image_preview::raster_row(
                &image.image,
                buf,
                Rect::new(area.x, area.y + i as u16, image.width.min(area.width), 1),
                image.width,
                image.height,
                image.y,
                mono,
            );
            continue;
        }
        let color = if (row.failed && row.anchor.line == 0)
            || row.text.trim_start().starts_with(['−', '✗'])
        {
            p.error
        } else if row.text.trim_start().starts_with('+')
            || (row.anchor.line > 0 && row.text.trim_start().starts_with('✓'))
        {
            p.success
        } else if row.dim {
            p.muted
        } else if row.anchor.line == 0 {
            match row.kind {
                Role::User => p.function,
                Role::Assistant => p.accent,
                Role::Tool => p.fg,
                Role::Thinking => p.muted,
                _ => p.fg,
            }
        } else {
            p.fg
        };
        let mut style = Style::default().fg(color).bg(p.bg);
        if row.anchor.line == 0 {
            style = style.add_modifier(Modifier::BOLD);
        }
        if app.focus == Focus::Transcript
            && Some(row.anchor.record) == selected
            && row.anchor.line == 0
        {
            style = style.bg(p.selected).add_modifier(Modifier::UNDERLINED);
        }
        if app
            .hover
            .is_some_and(|(x, y)| y == area.y + i as u16 && x >= area.x && x < area.right())
            && row.anchor.line == 0
            && matches!(row.kind, Role::Tool | Role::Thinking)
        {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        if row.summary && !row.text.is_empty() {
            label(
                buf,
                Rect::new(area.x, area.y + i as u16, 1, 1),
                "│",
                Style::default().fg(p.accent),
            );
        }
        let inset = row.inset;
        if row.summary {
            style = Style::default().fg(p.muted).add_modifier(Modifier::ITALIC);
        }
        paint_row(
            buf,
            Rect::new(
                area.x + inset,
                area.y + i as u16,
                area.width.saturating_sub(inset),
                1,
            ),
            row,
            style,
            p,
        );
        if row.anchor.line == 0 && row.text.starts_with('⠿') && app.preferences.motion {
            label(
                buf,
                row_rect(area, i as u16),
                ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
                    [app.animation_frame as usize % 10],
                Style::default().fg(p.accent),
            );
        }
    }
    if let Some(selection) = app.selection {
        for (i, row) in app
            .rows
            .iter()
            .skip(app.top)
            .take(area.height as usize)
            .enumerate()
        {
            for x in selection.row_cells(row) {
                if x >= width.saturating_sub(row.inset as usize) {
                    break;
                }
                if let Some(cell) = buf.cell_mut((area.x + x as u16 + row.inset, area.y + i as u16))
                {
                    cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }
    if app.rows.len() > area.height as usize {
        let track_x = area.right() - 1;
        let thumb = ((app.top as f64 / (app.rows.len() - area.height as usize).max(1) as f64)
            * (area.height.saturating_sub(1) as f64)) as u16;
        for y in 0..area.height {
            label(
                buf,
                Rect::new(track_x, area.y + y, 1, 1),
                if y == thumb { "┃" } else { "│" },
                Style::default().fg(if y == thumb { p.accent } else { p.border }),
            );
        }
    }
}
#[derive(PartialEq, Eq)]
struct InspectorKey {
    record: u64,
    revision: u64,
    width: usize,
    basic: bool,
    thinking: bool,
    compact: bool,
}

// One selected record, not an unbounded cache of every output and terminal width.
pub struct InspectorLayout {
    key: InspectorKey,
    rows: Vec<Row>,
}

fn draw_inspector(app: &mut App, buf: &mut Buffer, area: Rect, p: Palette, _mono: bool) {
    panel(buf, area, "Inspector · F3 Close", p);
    let inner = inset(area, 2, 2);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    if let Some(selected) = app.messages.get(app.selected) {
        let m = app
            .artifact
            .as_ref()
            .filter(|(owner, _)| *owner == selected.id)
            .map_or(selected, |(_, message)| message);
        let key = InspectorKey {
            record: m.id,
            revision: m.revision,
            width: inner.width as usize,
            basic: app.preferences.basic,
            thinking: app.preferences.thinking,
            compact: app.preferences.compact,
        };
        if app
            .inspector_cache
            .as_ref()
            .is_none_or(|cache| cache.key != key)
        {
            let mut preview = app.preferences.clone();
            preview.diff_split = false;
            let rows = text::inspector_rows(m, inner.width as usize, &preview);
            app.inspector_cache = Some(InspectorLayout { key, rows });
        }
        let Some(cache) = &app.inspector_cache else {
            return;
        };
        let rows = &cache.rows;
        app.inspector_scroll = app
            .inspector_scroll
            .min(rows.len().saturating_sub(inner.height as usize));
        for (i, row) in rows
            .iter()
            .skip(app.inspector_scroll)
            .take(inner.height as usize)
            .enumerate()
        {
            let inset = row.inset;
            if row.summary && !row.text.is_empty() {
                label(
                    buf,
                    row_rect(inner, i as u16),
                    "│",
                    Style::default().fg(p.accent),
                );
            }
            paint_row(
                buf,
                Rect::new(
                    inner.x + inset,
                    inner.y + i as u16,
                    inner.width.saturating_sub(inset),
                    1,
                ),
                row,
                p.style(),
                p,
            );
        }
    }
}

fn semantic_style(role: TextRole, p: Palette) -> Style {
    match role {
        TextRole::ToolTarget => Style::default().fg(p.fg).remove_modifier(Modifier::BOLD),
        TextRole::ToolName => Style::default().fg(p.key),
        TextRole::ToolError => Style::default().fg(p.error),
        TextRole::Underline => Style::default().add_modifier(Modifier::UNDERLINED),
        TextRole::Heading => Style::default().fg(p.heading).add_modifier(Modifier::BOLD),
        TextRole::Strong => Style::default().add_modifier(Modifier::BOLD),
        TextRole::Emphasis => Style::default().add_modifier(Modifier::ITALIC),
        TextRole::Strike => Style::default().add_modifier(Modifier::CROSSED_OUT),
        TextRole::InlineCode => Style::default().fg(p.code),
        TextRole::Link => Style::default()
            .fg(p.link)
            .add_modifier(Modifier::UNDERLINED),
        TextRole::CodeKeyword => Style::default().fg(p.accent),
        TextRole::CodeString => Style::default().fg(p.code),
        TextRole::CodeNumber => Style::default().fg(p.number),
        TextRole::CodeType => Style::default().fg(p.type_color),
        TextRole::CodeFunction => Style::default().fg(p.function),
        TextRole::DiffAdded => Style::default().fg(p.success),
        TextRole::DiffRemoved => Style::default().fg(p.error),
        TextRole::ListMarker => Style::default().fg(p.key),
        TextRole::CodeFence | TextRole::DiffGutter => Style::default().fg(p.border),
        TextRole::Quote => Style::default().fg(p.quote),
        TextRole::CodeComment => Style::default().fg(p.dim),
        TextRole::Muted => Style::default().fg(p.muted),
    }
}
fn paint_row(buf: &mut Buffer, r: Rect, row: &Row, base: Style, p: Palette) {
    if row.tree_stem && r.height > 0 {
        label(
            buf,
            Rect::new(r.x.saturating_sub(row.inset) + 2, r.y, 1, 1),
            "│",
            Style::default().fg(p.muted),
        );
    }
    let mut bounds = vec![0, row.text.len()];
    for span in &row.spans {
        bounds.extend([span.range.start, span.range.end]);
    }
    bounds.sort_unstable();
    bounds.dedup();
    let spans = bounds
        .windows(2)
        .filter_map(|pair| {
            let part = row.text.get(pair[0]..pair[1])?;
            let mut style = base;
            for span in &row.spans {
                if span.range.start <= pair[0] && span.range.end >= pair[1] {
                    style = style.patch(semantic_style(span.role, p));
                }
            }
            Some(Span::styled(part, style))
        })
        .collect();
    line(buf, r, spans);
}
fn draw_palette(
    app: &App,
    buf: &mut Buffer,
    area: Rect,
    p: Palette,
    choice: (&str, &str, usize),
    g: &mut Geometry,
) -> (Rect, Option<(u16, u16)>) {
    let (kind, query, selected) = choice;
    let scenes = false;
    let choices = app.choices(kind, query);
    let r = if is_config_palette(kind) {
        bottom_panel(
            area,
            if app.preferences.footer && area.height >= 16 {
                2
            } else {
                0
            },
            dock_request(app).unwrap_or(12),
        )
    } else {
        let width = if scenes {
            108
        } else {
            choices
                .iter()
                .map(|(_, s)| s.width() + 8)
                .max()
                .unwrap_or(48)
                .clamp(48, 100) as u16
        };
        let height = (choices.len() + 8)
            .max(if scenes { 18 } else { 10 })
            .min(area.height.saturating_sub(4) as usize) as u16;
        center(area, width, height)
    };
    if is_config_palette(kind) {
        dock_panel(
            buf,
            r,
            match kind.strip_prefix("manage:").unwrap_or(kind) {
                "sessions" => "Saved sessions",
                "selected-session" => "Selected session",
                "tree" => "Session tree",
                "fork-tree" => "Select source ancestry",
                "auth" => "Authentication",
                "router" => "Model router",
                "resources" => "Resources",
                "background" => "Notes and cache warming",
                "trust" => "Project trust",
                "updates" => "Updates",
                "copy-preview" => "Copy preview",
                "export-preview" => "Export preview",
                "plugins" => "Plugins",
                "models" => "Models",
                "settings" => "Settings",
                "connection" => "Connection",
                "delivery" => "Export and sharing",
                "live" => "Plugin views",
                "queue" => "Queued messages",
                _ => "Commands",
            },
            p,
        );
    } else {
        panel(buf, r, if scenes { "Scenario browser" } else { kind }, p);
    }
    let inner = inset(
        r,
        if is_config_palette(kind) { 2 } else { 4 },
        if is_config_palette(kind) { 1 } else { 2 },
    );
    if inner.width < 1 || inner.height < 3 {
        return (r, None);
    }
    let close = close_button(r);
    label(buf, close, " × ", Style::default().fg(p.muted));
    hit(g, close, HitAction::Close);
    line(
        buf,
        inner,
        vec![
            Span::styled("⌕  ", Style::default().fg(p.accent)),
            Span::styled(
                if query.is_empty() {
                    "Search by name, ID or category"
                } else {
                    query
                },
                Style::default().fg(if query.is_empty() { p.muted } else { p.fg }),
            ),
        ],
    );
    let compact = inner.height < 6;
    let list_top = if compact { 1 } else { 2 };
    let footer_rows = if compact { 1 } else { 2 };
    let cap = inner.height.saturating_sub(list_top + footer_rows).max(1) as usize;
    let start = selected.saturating_sub(cap.saturating_sub(1));
    let split = scenes && inner.width >= 80;
    let list_width = if split {
        inner.width * 55 / 100
    } else {
        inner.width
    };
    let current_category = String::new();
    for (i, (_id, name)) in choices.iter().enumerate().skip(start).take(cap) {
        let rr = Rect::new(
            inner.x,
            inner.y + list_top + (i - start) as u16,
            list_width,
            1,
        );
        let style = Style::default().fg(if i == selected { p.accent } else { p.fg });
        let style = if i == selected {
            style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            style
        };
        label(
            buf,
            rr,
            &format!("{} {}", if i == selected { "›" } else { " " }, name),
            style,
        );
        hit(g, rr, HitAction::Choice(i));
    }
    if choices.is_empty() {
        label(
            buf,
            row_rect(inner, list_top),
            "No matches. Try a shorter query.",
            Style::default().fg(p.muted),
        );
    }
    let count = if choices.is_empty() {
        "0 results".into()
    } else {
        format!("{} / {}", selected + 1, choices.len())
    };
    if !compact {
        label(
            buf,
            row_rect(inner, inner.height - 2),
            &format!("{count}  {current_category}"),
            Style::default().fg(p.muted),
        );
    }
    label(
        buf,
        row_rect(inner, inner.height - 1),
        if compact {
            "↑↓  Enter  Esc"
        } else if scenes {
            "↑↓ select  Tab category  Enter open  Esc back"
        } else {
            "↑↓ select  Enter open  Esc back"
        },
        Style::default().fg(p.muted),
    );
    (
        r,
        Some((
            inner.x + (3 + query.width()).min(inner.width.saturating_sub(1) as usize) as u16,
            inner.y,
        )),
    )
}
fn draw_settings(
    app: &App,
    buf: &mut Buffer,
    area: Rect,
    p: Palette,
    selected: usize,
    g: &mut Geometry,
) -> (Rect, Option<(u16, u16)>) {
    let footer = if app.preferences.footer && area.height >= 16 {
        2
    } else {
        0
    };
    let r = bottom_panel(area, footer, 12);
    Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(p.border))
        .render(r, buf);
    let inner = inset(r, 2, 1);
    if inner.height < 4 || inner.width < 1 {
        return (r, None);
    }
    let close = close_button(r);
    label(buf, close, " × ", Style::default().fg(p.muted));
    hit(g, close, HitAction::Close);
    label(
        buf,
        Rect::new(r.x + 2, r.y, r.width.saturating_sub(6), 1),
        if app.preferences.light {
            " /style · Light foreground "
        } else {
            " /style · GrokNight "
        },
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
    );
    let group = selected / 3;
    for (i, name) in ["Style", "Content", "Input"].iter().enumerate() {
        let rr = Rect::new(
            inner.x + i as u16 * 12,
            inner.y,
            10.min(inner.width.saturating_sub(i as u16 * 12)),
            1,
        );
        if rr.width == 0 {
            continue;
        }
        label(
            buf,
            rr,
            name,
            Style::default()
                .fg(if group == i { p.accent } else { p.muted })
                .add_modifier(if group == i {
                    Modifier::BOLD | Modifier::UNDERLINED
                } else {
                    Modifier::empty()
                }),
        );
        hit(g, rr, HitAction::SettingTab(i));
    }
    let values = [
        (
            "Light foreground",
            app.preferences.light,
            "Adapt text colours to a light terminal. The background stays yours.",
        ),
        (
            "Compact tools",
            app.preferences.compact,
            "Show concise tool summaries. Expand any result to read it in full.",
        ),
        (
            "Plain text",
            app.preferences.basic,
            "Use the basic renderer without rich formatting.",
        ),
        (
            "Split diff",
            app.preferences.diff_split,
            "Place before and after side by side when space allows.",
        ),
        (
            "Thinking preview",
            app.preferences.thinking,
            "Show a short reasoning preview, with the full content on demand.",
        ),
        (
            "Status footer",
            app.preferences.footer,
            "Show the model, context budget and session beneath the editor.",
        ),
        (
            "Mouse capture",
            app.preferences.mouse,
            "Enable clicking, dragging and scrolling. Turn off for terminal selection.",
        ),
        (
            "Motion",
            app.preferences.motion,
            "Animate active work. Input and streamed text remain immediate.",
        ),
        (
            "Editor line numbers",
            app.editor.style == 1,
            "Show line numbers without changing the draft or editing history.",
        ),
    ];
    for (i, (name, on, _)) in values.iter().enumerate().skip(group * 3).take(3) {
        let y = inner.y + 2 + (i % 3) as u16;
        if y >= inner.bottom().saturating_sub(1) {
            break;
        }
        let rr = Rect::new(inner.x, y, inner.width, 1);
        line(
            buf,
            rr,
            vec![
                Span::styled(
                    format!("{} {:<24}", if i == selected { "›" } else { " " }, name),
                    Style::default().fg(if i == selected { p.accent } else { p.fg }),
                ),
                Span::styled(
                    if *on { "on" } else { "off" },
                    Style::default().fg(if *on { p.success } else { p.muted }),
                ),
            ],
        );
        hit(g, rr, HitAction::Setting(i));
    }
    if inner.height > 8 {
        Paragraph::new(values[selected.min(8)].2)
            .style(Style::default().fg(p.muted))
            .wrap(Wrap { trim: false })
            .render(
                Rect::new(
                    inner.x,
                    inner.y + 7,
                    inner.width,
                    inner.height.saturating_sub(9),
                ),
                buf,
            );
    }
    label(
        buf,
        row_rect(inner, inner.height - 1),
        "Tab section  ↑↓ select  Space change  Esc back",
        Style::default().fg(p.muted),
    );
    (r, None)
}

fn bottom_panel(area: Rect, footer: u16, wanted: u16) -> Rect {
    let wanted = if area.height < 12 {
        area.height.saturating_sub(footer)
    } else {
        wanted
    };
    let reserve = if area.height < 12 { 0 } else { 3 };
    let height = wanted
        .min(area.height.saturating_sub(footer + reserve))
        .max(area.height.saturating_sub(footer).min(3));
    let pad = if area.width >= 90 { 2 } else { 1 };
    Rect::new(
        area.x + pad,
        area.bottom().saturating_sub(footer + height),
        area.width.saturating_sub(pad * 2),
        height,
    )
}
fn draw_completion(app: &App, buf: &mut Buffer, area: Rect, p: Palette, g: &mut Geometry) {
    if area.height < 1 || area.width < 1 {
        return;
    }
    Clear.render(area, buf);
    buf.set_style(area, p.style());
    let cap = area.height.saturating_sub(1).max(1) as usize;
    let start = app
        .autocomplete
        .selected
        .saturating_sub(cap.saturating_sub(1));
    let label_width = (area.width / 2).min(34) as usize;
    for (i, item) in app
        .autocomplete
        .items
        .iter()
        .enumerate()
        .skip(start)
        .take(cap)
    {
        let r = row_rect(area, (i - start) as u16);
        let selected = i == app.autocomplete.selected;
        let label_text = text::clipped(&item.label, label_width.saturating_sub(2));
        line(
            buf,
            r,
            vec![
                Span::styled(
                    format!(
                        "{} {}{}",
                        if selected { "›" } else { " " },
                        label_text,
                        " ".repeat(label_width.saturating_sub(label_text.width() + 2))
                    ),
                    Style::default().fg(if selected { p.accent } else { p.fg }),
                ),
                Span::styled(
                    format!("  {}", item.description),
                    Style::default().fg(p.muted),
                ),
            ],
        );
        hit(g, r, HitAction::Completion(i));
    }
    if app.autocomplete.items.is_empty() {
        label(
            buf,
            area,
            if app.autocomplete.pending {
                "Searching files…"
            } else {
                "No matching entries"
            },
            Style::default().fg(p.muted),
        );
    }
    if area.height > 1 {
        label(
            buf,
            row_rect(area, area.height - 1),
            &format!(
                "{} / {}   Tab or Enter select · Esc close",
                if app.autocomplete.items.is_empty() {
                    0
                } else {
                    app.autocomplete.selected + 1
                },
                app.autocomplete.items.len()
            ),
            Style::default().fg(p.muted),
        );
    }
}

fn is_config_palette(kind: &str) -> bool {
    kind.starts_with("manage:")
        || matches!(
            kind,
            "settings"
                | "plugins"
                | "models"
                | "connection"
                | "delivery"
                | "live"
                | "commands"
                | "queue"
        )
}
fn dock_request(app: &App) -> Option<u16> {
    match app.dialog.as_ref()? {
        Dialog::Settings { .. } => Some(12),
        Dialog::Form {
            fields, selected, ..
        } => Some(
            (fields
                .get(*selected)
                .filter(|f| f.picker.is_some())
                .map_or(fields.len() + 6, |f| f.choices().len() + 5))
            .clamp(7, 16) as u16,
        ),
        Dialog::Palette { kind, query, .. } if is_config_palette(kind) => {
            Some((app.choices(kind, query).len() + 6).clamp(7, 16) as u16)
        }
        _ => None,
    }
}
fn help_line(text: &str, p: Palette) -> Line<'static> {
    let spans = text
        .split_inclusive(char::is_whitespace)
        .map(|word| {
            let token = word.trim();
            let command = token.starts_with('/');
            let key = token.starts_with("Ctrl")
                || token.starts_with("Alt")
                || token.starts_with("Shift")
                || (token.starts_with('F')
                    && token.chars().nth(1).is_some_and(|c| c.is_ascii_digit()))
                || matches!(token, "Enter" | "Esc" | "Tab" | "PgUp/PgDn");
            let mut style = Style::default().fg(if command {
                p.accent
            } else if key {
                p.key
            } else if token == "·" {
                p.dim
            } else {
                p.fg
            });
            if command || key {
                style = style.add_modifier(Modifier::BOLD);
            }
            Span::styled(word.to_owned(), style)
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

const HELP_LINES: &[&str] = &[
    "Enter Send · Shift+Enter / Ctrl+J Newline · Tab Complete",
    "Esc Close local view, then cancel foreground · Ctrl+D Detach empty composer",
    "Ctrl+C Copy selection, otherwise clear draft · /quit Detach",
    "Alt+S Steering · Alt+F Follow-up · /queue Review and withdraw",
    "Ctrl+G External editor · Ctrl+Z Undo · Ctrl+Alt+Z Suspend (Unix)",
    "Ctrl+F Search loaded branch · PgUp/PgDn Read · Ctrl+End Latest",
    "F2 Commands · F3 Inspector · F4 Turns · F6 Focus · F7 Context · F10 Style",
    "Ctrl+O Expand tool · Ctrl+T Thinking · /copy Last answer",
    "@file Content snapshot · /attachments Review / refresh / remove",
    "!command Shell in context · !!command Shell excluded from context",
    "Ctrl+R Reconcile uncertain request · /stop-host Stop shared host",
];
fn draw_dialog(
    app: &mut App,
    buf: &mut Buffer,
    area: Rect,
    p: Palette,
    mono: bool,
    g: &mut Geometry,
) -> (Rect, Option<(u16, u16)>) {
    let result = draw_builtin_dialog(app, buf, area, p, mono, g);
    if app.overlay.is_none() {
        return result;
    }
    let Some(dialog) = &app.dialog else {
        return result;
    };
    let payload = match dialog {
        Dialog::Palette {
            kind,
            query,
            selected,
        } => {
            serde_json::json!({
                "kind": kind,
                "query": query,
                "selected": selected,
                "items": app.choices(kind, query),
            })
        }
        Dialog::Form {
            title,
            fields,
            selected,
            status,
        } => {
            serde_json::json!({
                "kind": "form",
                "title": title,
                "selected": selected,
                "status": status,
                "fields": fields
                    .iter()
                    .map(|f| serde_json::json!({
                        "key": f.key,
                        "label": f.label,
                        "value": f.display_value(),
                        "private": f.private,
                    }))
                    .collect::<Vec<_>>(),
            })
        }
        Dialog::Details { title, text } => {
            serde_json::json!({
                "kind": "details",
                "title": title,
                "text": text,
                "scroll": app.dialog_scroll,
            })
        }
        _ => serde_json::json!({ "kind": "help" }),
    };
    let mut payload = payload;
    payload["theme"] = serde_json::json!({ "foreground": rgb(p.fg), "background": rgb(p.bg) });
    if let Some(overlay) = &mut app.overlay {
        match overlay.render(&payload, buf, result.0, mono) {
            Ok(cursor) => (result.0, cursor.or(result.1)),
            Err(error) => {
                app.notice = format!("Overlay fallback: {error}");
                result
            }
        }
    } else {
        result
    }
}
fn draw_builtin_dialog(
    app: &mut App,
    buf: &mut Buffer,
    area: Rect,
    p: Palette,
    _mono: bool,
    g: &mut Geometry,
) -> (Rect, Option<(u16, u16)>) {
    let Some(dialog) = app.dialog.clone() else {
        return (area, None);
    };
    match &dialog {
        Dialog::Palette {
            kind,
            query,
            selected,
        } => return draw_palette(app, buf, area, p, (kind, query, *selected), g),
        Dialog::Settings { selected } => return draw_settings(app, buf, area, p, *selected, g),
        _ => {}
    }
    let is_form = matches!(dialog, Dialog::Form { .. });
    let r = if is_form {
        bottom_panel(
            area,
            if app.preferences.footer && area.height >= 16 {
                2
            } else {
                0
            },
            dock_request(app).unwrap_or(12),
        )
    } else {
        let width = (HELP_LINES.iter().map(|s| s.width()).max().unwrap_or(40) + 8)
            .min(area.width.saturating_sub(4).max(1) as usize);
        let height = HELP_LINES
            .iter()
            .map(|s| text::wrap(s, width.saturating_sub(8).max(1)).len())
            .sum::<usize>()
            + 4;
        center(area, width as u16, height.min(u16::MAX as usize) as u16)
    };
    let title = match &dialog {
        Dialog::Form { title, .. } | Dialog::Details { title, .. } => title.as_str(),
        _ => "Keyboard shortcuts",
    };
    if is_form {
        dock_panel(buf, r, title, p)
    } else {
        panel(buf, r, title, p)
    }
    hit(g, close_button(r), HitAction::Close);
    label(buf, close_button(r), " × ", Style::default().fg(p.muted));
    let inner = inset(r, if is_form { 2 } else { 4 }, if is_form { 1 } else { 2 });
    if inner.width == 0 || inner.height == 0 {
        return (r, None);
    }
    if let Dialog::Form {
        fields,
        selected,
        status,
        ..
    } = dialog
    {
        let form = crate::fields::render(buf, inner, &fields, selected, &status, p);
        for (index, r) in form.fields {
            hit(g, r, HitAction::Field(index));
        }
        for (index, r) in form.choices {
            hit(g, r, HitAction::FieldChoice(index));
        }
        if let Some(r) = form.apply {
            hit(g, r, HitAction::Apply)
        }
        return (r, form.cursor);
    }
    let detail_lines;
    let source = if let Dialog::Details { text, .. } = &dialog {
        detail_lines = text.lines().collect::<Vec<_>>();
        detail_lines.as_slice()
    } else {
        HELP_LINES
    };
    let lines: Vec<_> = source
        .iter()
        .flat_map(|s| text::wrap(s, inner.width as usize))
        .map(|(_, s)| help_line(&s, p))
        .collect();
    app.dialog_scroll = app
        .dialog_scroll
        .min(lines.len().saturating_sub(inner.height as usize));
    Paragraph::new(lines)
        .scroll((app.dialog_scroll.min(u16::MAX as usize) as u16, 0))
        .render(inner, buf);
    (r, None)
}

fn rgb(color: Color) -> u32 {
    match color {
        Color::Rgb(r, g, b) => (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
        _ => eden_ui_sdk::COLOR_DEFAULT,
    }
}
