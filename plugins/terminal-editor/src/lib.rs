//! Typed editor and overlay renderer behind a C ABI adapter.
use eden_ui_sdk::{
    ABI_VERSION, ByteSink, CELL_BOLD, CELL_CURSOR, CELL_REVERSE, COLOR_DEFAULT, Cell, CellSink,
    EVENT_ERROR, UiApi,
};
use std::{
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const MAX_TEXT: usize = 4 * 1024 * 1024;

#[derive(Clone, Default)]
struct Editor {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
    numbered: bool,
    undo: Vec<Checkpoint>,
    redo: Vec<Checkpoint>,
    kill: String,
    kill_ring: Vec<String>,
    yank: Option<(std::ops::Range<usize>, usize)>,
    layout_rows: Vec<Vec<(u16, usize)>>,
    preferred_column: Option<usize>,
    scroll: usize,
    pointer_rows: Vec<Vec<(u16, usize)>>,
    pointer_gutter: u16,
}

#[derive(Clone)]
struct Checkpoint {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
}

impl Editor {
    fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            text: self.text.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
        }
    }

    fn apply_checkpoint(&mut self, point: Checkpoint) {
        self.text = point.text;
        self.cursor = point.cursor;
        self.anchor = point.anchor;
        self.preferred_column = None;
        self.pointer_rows.clear();
        self.layout_rows.clear();
    }

    fn selection(&self) -> std::ops::Range<usize> {
        let anchor = self.anchor.unwrap_or(self.cursor);
        anchor.min(self.cursor)..anchor.max(self.cursor)
    }

    fn selected_text(&self) -> &str {
        &self.text[self.selection()]
    }

    fn previous(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next(&self) -> usize {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |g| self.cursor + g.len())
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i)
    }

    fn word_left(&self) -> usize {
        let mut iter = self.text[..self.cursor]
            .grapheme_indices(true)
            .rev()
            .peekable();
        let mut target = self.cursor;
        while let Some(&(i, g)) = iter.peek() {
            if !g.chars().all(char::is_whitespace) {
                break;
            }
            target = i;
            iter.next();
        }
        let Some((i, g)) = iter.next() else {
            return target;
        };
        target = i;
        let word = g.chars().any(|c| c.is_alphanumeric() || c == '_');
        for (i, g) in iter {
            if g.chars().all(char::is_whitespace)
                || g.chars().any(|c| c.is_alphanumeric() || c == '_') != word
            {
                break;
            }
            target = i;
        }
        target
    }

    fn word_right(&self) -> usize {
        let mut iter = self.text[self.cursor..].grapheme_indices(true).peekable();
        let mut target = self.cursor;
        if let Some(&(_, first)) = iter.peek() {
            let word = first.chars().any(|c| c.is_alphanumeric() || c == '_');
            while let Some(&(i, g)) = iter.peek() {
                if g.chars().all(char::is_whitespace)
                    || g.chars().any(|c| c.is_alphanumeric() || c == '_') != word
                {
                    break;
                }
                target = self.cursor + i + g.len();
                iter.next();
            }
        }
        for (i, g) in iter {
            if !g.chars().all(char::is_whitespace) {
                break;
            }
            target = self.cursor + i + g.len();
        }
        target
    }

    fn vertical(&mut self, down: bool) -> usize {
        if let Some((row, column)) =
            self.layout_rows
                .iter()
                .enumerate()
                .rev()
                .find_map(|(row, stops)| {
                    stops
                        .iter()
                        .find(|(_, byte)| *byte == self.cursor)
                        .map(|(column, _)| (row, *column))
                })
        {
            let preferred = *self.preferred_column.get_or_insert(column as usize);
            let target = if down { row + 1 } else { row.saturating_sub(1) };
            if let Some(stops) = self.layout_rows.get(target) {
                return stops
                    .iter()
                    .rev()
                    .find(|(x, _)| *x as usize <= preferred)
                    .or_else(|| stops.first())
                    .map_or(self.cursor, |(_, byte)| *byte);
            }
            return self.cursor;
        }

        let start = self.line_start();
        let column = *self
            .preferred_column
            .get_or_insert_with(|| self.text[start..self.cursor].width());
        let target = if down {
            let end = self.line_end();
            if end == self.text.len() {
                return self.cursor;
            }
            end + 1
        } else {
            if start == 0 {
                return self.cursor;
            }
            self.text[..start - 1].rfind('\n').map_or(0, |i| i + 1)
        };
        let mut cursor = target;
        let mut width = 0;
        for g in self.text[target..]
            .split('\n')
            .next()
            .unwrap_or("")
            .graphemes(true)
        {
            if width + g.width() > column {
                break;
            }
            width += g.width();
            cursor += g.len();
        }
        cursor
    }

    fn replace(&mut self, range: std::ops::Range<usize>, text: &str) -> Result<(), ()> {
        let normalized = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ");
        let clean: String = normalized
            .chars()
            .filter(|c| *c == '\n' || !c.is_control())
            .collect();
        if self.text.len() - range.len() + clean.len() > MAX_TEXT {
            return Err(());
        }
        if range.is_empty() && clean.is_empty() {
            return Ok(());
        }
        self.undo.push(self.checkpoint());
        if self.undo.len() > 128 {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.cursor = range.start + clean.len();
        self.text.replace_range(range, &clean);
        // Insertion/deletion can merge adjacent combining sequences.
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .find(|&i| i >= self.cursor)
            .unwrap_or(self.text.len());
        self.anchor = None;
        self.preferred_column = None;
        self.pointer_rows.clear();
        self.layout_rows.clear();
        Ok(())
    }

    fn event(&mut self, kind: u32, key: u32, mods: u32, text: &str) -> Result<(), ()> {
        if kind != 0 || key != 20 {
            self.yank = None;
        }
        if kind == 3 {
            let (start, end) = (key as usize, mods as usize);
            let boundary = |i| {
                i == self.text.len() || self.text.grapheme_indices(true).any(|(byte, _)| byte == i)
            };
            if start > end || end > self.text.len() || !boundary(start) || !boundary(end) {
                return Err(());
            }
            return self.replace(start..end, text);
        }
        if kind == 1 {
            return self.replace(self.selection(), text);
        }
        if kind == 2 {
            let column = (key as u16).saturating_sub(self.pointer_gutter);
            let row = (key >> 16) as usize;
            let Some(stops) = self.pointer_rows.get(row) else {
                return Err(());
            };
            let target = stops
                .iter()
                .rev()
                .find(|(x, _)| *x <= column)
                .or_else(|| stops.first())
                .map_or(self.text.len(), |(_, byte)| *byte);
            if mods & 1 != 0 {
                self.anchor.get_or_insert(self.cursor);
            } else {
                self.anchor = None;
            }
            self.cursor = target;
            self.preferred_column = None;
            return Ok(());
        }
        if kind != 0 {
            return Err(());
        }
        let shift = mods & 1 != 0;
        let ctrl = mods & 2 != 0;
        if matches!(key, 1..=6 | 17 | 18) {
            let selection = self.selection();
            let target = match key {
                1 if ctrl => self.word_left(),
                2 if ctrl => self.word_right(),
                1 if !shift && !selection.is_empty() => selection.start,
                2 if !shift && !selection.is_empty() => selection.end,
                1 => self.previous(),
                2 => self.next(),
                3 => self.vertical(false),
                4 => self.vertical(true),
                5 if ctrl => 0,
                6 if ctrl => self.text.len(),
                5 => self.line_start(),
                6 => self.line_end(),
                17 => self.word_left(),
                18 => self.word_right(),
                _ => self.cursor,
            };
            if shift {
                self.anchor.get_or_insert(self.cursor);
            } else {
                self.anchor = None;
            }
            self.cursor = target;
            if !matches!(key, 3 | 4) {
                self.preferred_column = None;
            }
            return Ok(());
        }
        match key {
            7 | 8 | 14 | 15 | 19 => {
                let selection = self.selection();
                let range = if !selection.is_empty() {
                    selection
                } else {
                    match key {
                        7 if ctrl => self.word_left()..self.cursor,
                        7 => self.previous()..self.cursor,
                        8 if ctrl => self.cursor..self.word_right(),
                        8 => self.cursor..self.next(),
                        14 => self.word_left()..self.cursor,
                        15 => {
                            self.cursor..if self.cursor == self.line_end() {
                                self.next()
                            } else {
                                self.line_end()
                            }
                        }
                        19 => self.line_start()..self.cursor,
                        _ => self.cursor..self.cursor,
                    }
                };
                if matches!(key, 14 | 15 | 19) && !range.is_empty() {
                    self.kill = self.text[range.clone()].to_owned();
                    self.kill_ring.insert(0, self.kill.clone());
                    self.kill_ring.truncate(32);
                }
                self.replace(range, "")?;
            }
            9 => self.replace(self.selection(), "\n")?,
            10 if mods & 6 == 0 => self.replace(self.selection(), text)?,
            11 => {
                self.anchor = Some(0);
                self.cursor = self.text.len();
                self.preferred_column = None;
            }
            12 => {
                if let Some(point) = self.undo.pop() {
                    self.redo.push(self.checkpoint());
                    self.apply_checkpoint(point);
                }
            }
            13 => {
                if let Some(point) = self.redo.pop() {
                    self.undo.push(self.checkpoint());
                    self.apply_checkpoint(point);
                }
            }
            16 => {
                let kill = self.kill.clone();
                let start = self.selection().start;
                self.replace(self.selection(), &kill)?;
                self.yank = Some((start..self.cursor, 0));
            }
            20 => {
                if let Some((range, index)) = self.yank.clone()
                    && !self.kill_ring.is_empty()
                    && self
                        .text
                        .grapheme_indices(true)
                        .any(|(byte, _)| byte == range.start)
                {
                    let next = (index + 1) % self.kill_ring.len();
                    let text = self.kill_ring[next].clone();
                    let start = range.start;
                    self.replace(range, &text)?;
                    self.yank = Some((start..self.cursor, next));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn restore(&mut self, text: &str, cursor: usize) -> Result<(), ()> {
        let boundary =
            cursor == text.len() || text.grapheme_indices(true).any(|(i, _)| i == cursor);
        if text.len() > MAX_TEXT || !boundary || text.chars().any(|c| c.is_control() && c != '\n') {
            return Err(());
        }
        let numbered = self.numbered;
        *self = Self {
            text: text.to_owned(),
            cursor,
            numbered,
            ..Self::default()
        };
        Ok(())
    }

    fn render(
        &mut self,
        width: u16,
        height: u16,
        mode: u32,
        dark: bool,
        sink: CellSink,
        ctx: *mut c_void,
    ) {
        if width == 0 || height == 0 {
            if mode == 0 {
                self.pointer_rows.clear();
                self.layout_rows.clear();
            }
            return;
        }
        let palette = if dark {
            Palette {
                fg: eden_ui_sdk::mocha::TEXT,
                bg: COLOR_DEFAULT,
                accent: eden_ui_sdk::mocha::MAUVE,
                muted: eden_ui_sdk::mocha::SUBTEXT,
            }
        } else {
            Palette {
                fg: 0x25232b,
                bg: COLOR_DEFAULT,
                accent: 0x673699,
                muted: 0x686473,
            }
        };
        let mut canvas = Canvas {
            width,
            height,
            palette,
            sink,
            ctx,
        };
        for y in 0..height {
            for x in 0..width {
                canvas.emit(x, y, " ", palette.fg, 0);
            }
        }
        if mode == 1 {
            for (y, line) in [
                "Actions",
                "",
                "[j] Jump to latest",
                "[f] Fold selected message",
                "[c] Copy selection",
                "[p] Switch editor style",
                "[t] Switch theme",
                "[Esc] Close",
            ]
            .iter()
            .enumerate()
            {
                canvas.line(
                    1,
                    y as u16,
                    line,
                    if y == 0 { palette.accent } else { palette.fg },
                    if y == 0 { CELL_BOLD } else { 0 },
                );
            }
            return;
        }
        let gutter = if (self.numbered || mode == 2) && width >= 9 {
            4
        } else {
            0
        };
        let content_width = width - gutter;
        let mut placements = Vec::new();
        let mut pointer_rows = vec![vec![(0, 0)]];
        let mut x: u16 = 0;
        let mut y: usize = 0;
        let mut line_number: usize = 1;
        let mut row_labels = vec![(0, line_number)];
        let mut cursor = (0, 0);
        let mut wrapped = false;
        for (byte, grapheme) in self.text.grapheme_indices(true) {
            let cell_width = grapheme.width().max(1).min(u16::MAX as usize) as u16;
            if grapheme != "\n" && x > 0 && x.saturating_add(cell_width) > content_width {
                x = 0;
                y += 1;
                pointer_rows.push(vec![(0, byte)]);
            }
            if byte == self.cursor {
                cursor = (x, y);
            }
            if grapheme == "\n" {
                x = 0;
                // A line that exactly filled the viewport already advanced a row.
                if !wrapped {
                    y += 1;
                    pointer_rows.push(vec![(0, byte + grapheme.len())]);
                } else {
                    pointer_rows[y] = vec![(0, byte + grapheme.len())];
                }
                wrapped = false;
                line_number += 1;
                row_labels.push((y, line_number));
                continue;
            }
            if cell_width <= content_width {
                placements.push((x, y, byte, grapheme));
            }
            x = x.saturating_add(cell_width);
            pointer_rows[y].push((x.min(content_width), byte + grapheme.len()));
            wrapped = x >= content_width;
            if x >= content_width {
                x = 0;
                y += 1;
                pointer_rows.push(vec![(0, byte + grapheme.len())]);
            }
        }
        if self.cursor == self.text.len() {
            cursor = (x, y);
        }
        if mode == 0 {
            if cursor.1 < self.scroll {
                self.scroll = cursor.1;
            } else if cursor.1 >= self.scroll + height as usize {
                self.scroll = cursor.1 + 1 - height as usize;
            }
        }
        let scroll = if mode == 2 { 0 } else { self.scroll };
        if mode == 0 {
            self.layout_rows = pointer_rows.clone();
            self.pointer_gutter = gutter;
            self.pointer_rows = pointer_rows
                .into_iter()
                .skip(scroll)
                .take(height as usize)
                .collect();
            self.pointer_rows
                .resize_with(height as usize, || vec![(0, self.text.len())]);
        }
        let selection = self.selection();
        for (x, y, byte, text) in placements {
            if y >= scroll && y - scroll < height as usize {
                let selected = selection.contains(&byte);
                let fg = if mode == 2 && text.chars().all(|c| c.is_ascii_digit()) {
                    palette.accent
                } else {
                    palette.fg
                };
                canvas.emit(
                    x + gutter,
                    (y - scroll) as u16,
                    text,
                    fg,
                    if selected { CELL_REVERSE } else { 0 },
                );
            }
        }
        if gutter > 0 {
            for (y, number) in row_labels {
                if y >= scroll && y - scroll < height as usize {
                    canvas.line(
                        0,
                        (y - scroll) as u16,
                        &format!("{:>3} ", number % 1000),
                        palette.muted,
                        0,
                    );
                }
            }
        }
        if mode == 2 {
            return;
        }
        canvas.emit(
            cursor.0 + gutter,
            (cursor.1 - scroll) as u16,
            "",
            palette.accent,
            CELL_CURSOR,
        );
    }
}

#[derive(Clone, Copy)]
struct Palette {
    fg: u32,
    bg: u32,
    accent: u32,
    muted: u32,
}

struct Canvas {
    width: u16,
    height: u16,
    palette: Palette,
    sink: CellSink,
    ctx: *mut c_void,
}

impl Canvas {
    fn emit(&mut self, x: u16, y: u16, text: &str, fg: u32, flags: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        (self.sink)(
            self.ctx,
            &Cell {
                x,
                y,
                fg,
                bg: self.palette.bg,
                flags,
                text: text.as_ptr(),
                text_len: text.len(),
            },
        );
    }

    fn line(&mut self, mut x: u16, y: u16, text: &str, fg: u32, flags: u32) {
        for g in text.graphemes(true) {
            let w = g.width().max(1).min(u16::MAX as usize) as u16;
            if x.saturating_add(w) > self.width {
                break;
            }
            self.emit(x, y, g, fg, flags);
            x += w;
        }
    }
}

// Foreign callers must obey the SDK pointer and lifetime contract. Null and input
// lengths are checked here; arbitrary dangling pointers cannot be validated in-process.
unsafe fn input<'a>(ptr: *const u8, len: usize) -> Result<&'a str, ()> {
    if len == 0 {
        return Ok("");
    }
    if ptr.is_null() || len > MAX_TEXT {
        return Err(());
    }
    // SAFETY: the serialized SDK call keeps its library, handle and borrowed buffers live; tests create these values locally.
    std::str::from_utf8(unsafe { std::slice::from_raw_parts(ptr, len) }).map_err(|_| ())
}

unsafe extern "C" fn create(style: u32) -> *mut c_void {
    catch_unwind(|| {
        if style > 1 {
            return std::ptr::null_mut();
        }
        Box::into_raw(Box::new(Editor {
            numbered: style == 1,
            ..Editor::default()
        }))
        .cast()
    })
    .unwrap_or(std::ptr::null_mut())
}

unsafe extern "C" fn destroy(state: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !state.is_null() {
            // SAFETY: destroy consumes the live, uniquely owned handle from create.
            drop(unsafe { Box::from_raw(state.cast::<Editor>()) });
        }
    }));
}

unsafe extern "C" fn event(
    state: *mut c_void,
    kind: u32,
    key: u32,
    mods: u32,
    data: *const u8,
    len: usize,
) -> u32 {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the serialized SDK call keeps its library, handle and borrowed buffers live; tests create these values locally.
        let Some(editor) = (unsafe { state.cast::<Editor>().as_mut() }) else {
            return EVENT_ERROR;
        };
        let text = if kind == 1 || kind == 3 || (kind == 0 && key == 10) {
            // SAFETY: the serialized SDK call keeps its library, handle and borrowed buffers live; tests create these values locally.
            match unsafe { input(data, len) } {
                Ok(text) => text,
                Err(()) => return EVENT_ERROR,
            }
        } else {
            ""
        };
        editor
            .event(kind, key, mods, text)
            .map_or(EVENT_ERROR, |()| 0)
    }))
    .unwrap_or(EVENT_ERROR)
}

unsafe extern "C" fn render(
    state: *mut c_void,
    width: u16,
    height: u16,
    mode: u32,
    dark: u32,
    sink: CellSink,
    ctx: *mut c_void,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the serialized SDK call keeps its library, handle and borrowed buffers live; tests create these values locally.
        if let Some(editor) = unsafe { state.cast::<Editor>().as_mut() }
            && mode <= 2
        {
            editor.render(width, height, mode, dark != 0, sink, ctx);
        }
    }));
}

unsafe extern "C" fn snapshot(state: *mut c_void, sink: ByteSink, ctx: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the serialized SDK call keeps its library, handle and borrowed buffers live; tests create these values locally.
        if let Some(editor) = unsafe { state.cast::<Editor>().as_ref() } {
            sink(ctx, editor.text.as_ptr(), editor.text.len());
        }
    }));
}

unsafe extern "C" fn cursor(state: *mut c_void) -> usize {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the serialized SDK call keeps its library, handle and borrowed buffers live; tests create these values locally.
        unsafe { state.cast::<Editor>().as_ref() }.map_or(0, |editor| editor.cursor)
    }))
    .unwrap_or(0)
}

unsafe extern "C" fn restore(
    state: *mut c_void,
    data: *const u8,
    len: usize,
    cursor: usize,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the serialized SDK call keeps its library, handle and borrowed buffers live; tests create these values locally.
        let Some(editor) = (unsafe { state.cast::<Editor>().as_mut() }) else {
            return -1;
        };
        // SAFETY: the serialized SDK call keeps its library, handle and borrowed buffers live; tests create these values locally.
        match unsafe { input(data, len) } {
            Ok(text) => editor.restore(text, cursor).map_or(-1, |()| 0),
            Err(()) => -1,
        }
    }))
    .unwrap_or(-1)
}

unsafe extern "C" fn selected_text(state: *mut c_void, sink: ByteSink, ctx: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the serialized SDK call keeps its library, handle and borrowed buffers live; tests create these values locally.
        if let Some(editor) = unsafe { state.cast::<Editor>().as_ref() } {
            let text = editor.selected_text();
            sink(ctx, text.as_ptr(), text.len());
        }
    }));
}

unsafe extern "C" fn transfer_state(destination: *mut c_void, source: *mut c_void) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if destination.is_null() || source.is_null() {
            return -1;
        }
        if destination == source {
            return 0;
        }
        // Both handles belong to this loaded library; only plain C pointers cross the ABI.
        // SAFETY: the serialized SDK call keeps its library, handle and borrowed buffers live; tests create these values locally.
        let source = unsafe { &*source.cast::<Editor>() };
        // SAFETY: the serialized SDK call keeps its library, handle and borrowed buffers live; tests create these values locally.
        let destination = unsafe { &mut *destination.cast::<Editor>() };
        let numbered = destination.numbered;
        *destination = source.clone();
        destination.numbered = numbered;
        destination.pointer_rows.clear();
        destination.layout_rows.clear();
        0
    }))
    .unwrap_or(-1)
}

static API: UiApi = UiApi {
    abi: ABI_VERSION,
    table_size: std::mem::size_of::<UiApi>() as u32,
    create,
    destroy,
    event,
    render,
    snapshot,
    cursor,
    restore,
    selected_text,
    transfer_state,
};

#[unsafe(no_mangle)]
/// Returns a static table valid until this library is unloaded.
pub extern "C" fn eden_ui_v1() -> *const UiApi {
    &API
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(editor: &mut Editor, key: u32, mods: u32) {
        editor.event(0, key, mods, "").unwrap();
    }

    #[test]
    fn vertical_keys_follow_soft_wrapped_rows() {
        let mut editor = Editor::default();
        editor.restore("abcdefghij", 2).unwrap();
        draw(&mut editor, 4, 4, 0);
        key(&mut editor, 4, 0);
        assert_eq!(editor.cursor, 6);
        key(&mut editor, 4, 0);
        assert_eq!(editor.cursor, 10);
        key(&mut editor, 3, 0);
        assert_eq!(editor.cursor, 6);
    }

    #[test]
    fn yank_pop_cycles_previous_kills_and_stops_after_other_input() {
        let mut editor = Editor::default();
        editor.restore("one two", 7).unwrap();
        key(&mut editor, 14, 0);
        key(&mut editor, 14, 0);
        key(&mut editor, 16, 0);
        assert_eq!(editor.text, "one ");
        key(&mut editor, 20, 0);
        assert_eq!(editor.text, "two");
        key(&mut editor, 20, 0);
        assert_eq!(editor.text, "one ");
        key(&mut editor, 1, 0);
        key(&mut editor, 20, 0);
        assert_eq!(editor.text, "one ");
    }

    #[test]
    fn range_replacement_preserves_history_and_rejects_partial_graphemes() {
        let mut e = Editor::default();
        e.event(1, 0, 0, "中文 /set").unwrap();
        let before = e.text.clone();
        let start = "中文 ".len();
        e.event(3, start as u32, before.len() as u32, "/settings ")
            .unwrap();
        assert_eq!(e.text, "中文 /settings ");
        e.event(0, 12, 0, "").unwrap();
        assert_eq!(e.text, before);
        e.event(0, 12, 0, "").unwrap();
        assert_eq!(e.text, "");
        e.restore("e\u{301}中文", 0).unwrap();
        assert!(e.event(3, 1, 3, "x").is_err());
        assert_eq!(e.text, "e\u{301}中文");
    }
    #[test]
    fn unicode_selection_replaces_whole_graphemes_and_undo_restores_selection() {
        let mut editor = Editor::default();
        editor.event(1, 0, 0, "中👩‍💻e\u{301}").unwrap();
        key(&mut editor, 1, 1);
        key(&mut editor, 1, 1);
        assert_eq!(editor.selected_text(), "👩‍💻e\u{301}");
        editor.event(1, 0, 0, "新\r\nline").unwrap();
        assert_eq!(editor.text, "中新\nline");
        key(&mut editor, 12, 0);
        assert_eq!(editor.selected_text(), "👩‍💻e\u{301}");
        key(&mut editor, 13, 0);
        assert_eq!(editor.text, "中新\nline");
    }

    #[test]
    fn paste_is_one_undo_step_and_normalizes_terminal_controls() {
        let mut editor = Editor::default();
        editor.event(0, 10, 0, "a").unwrap();
        editor
            .event(1, 0, 0, "中\r\n👩‍💻\te\u{301}\u{1b}\u{7f}")
            .unwrap();
        assert_eq!(editor.text, "a中\n👩‍💻    e\u{301}");
        key(&mut editor, 12, 0);
        assert_eq!(editor.text, "a");
        key(&mut editor, 13, 0);
        assert_eq!(editor.text, "a中\n👩‍💻    e\u{301}");
    }

    #[test]
    fn ctrl_word_selection_delete_and_kill_yank_are_grapheme_safe() {
        let mut editor = Editor::default();
        editor.event(1, 0, 0, "one 中文 👩‍💻").unwrap();
        key(&mut editor, 1, 3);
        assert_eq!(editor.selected_text(), "👩‍💻");
        key(&mut editor, 14, 0);
        assert_eq!(editor.text, "one 中文 ");
        key(&mut editor, 16, 0);
        assert_eq!(editor.text, "one 中文 👩‍💻");
        key(&mut editor, 17, 0);
        key(&mut editor, 7, 2);
        assert_eq!(editor.text, "one 👩‍💻");
        key(&mut editor, 5, 2);
        key(&mut editor, 8, 2);
        assert_eq!(editor.text, "👩‍💻");
    }

    #[test]
    fn kill_end_can_join_lines_and_yank_restores_them() {
        let mut editor = Editor::default();
        editor.restore("abc\ndef", 3).unwrap();
        key(&mut editor, 15, 0);
        assert_eq!(editor.text, "abcdef");
        key(&mut editor, 16, 0);
        assert_eq!(editor.text, "abc\ndef");
        key(&mut editor, 6, 0);
        key(&mut editor, 19, 0);
        assert_eq!(editor.text, "abc\n");
        key(&mut editor, 16, 0);
        assert_eq!(editor.text, "abc\ndef");
    }

    #[test]
    fn ordinary_horizontal_movement_collapses_selection() {
        let mut editor = Editor::default();
        editor.restore("abc", 3).unwrap();
        key(&mut editor, 11, 0);
        key(&mut editor, 1, 0);
        assert_eq!(editor.cursor, 0);
        assert!(editor.selected_text().is_empty());
        key(&mut editor, 11, 0);
        key(&mut editor, 2, 0);
        assert_eq!(editor.cursor, 3);
    }

    #[test]
    fn vertical_movement_preserves_display_column_across_short_lines() {
        let mut editor = Editor::default();
        editor.restore("中文ab\nx\n123456", "中文".len()).unwrap();
        key(&mut editor, 4, 1);
        key(&mut editor, 4, 1);
        assert_eq!(&editor.text[..editor.cursor], "中文ab\nx\n1234");
        assert_eq!(editor.selected_text(), "ab\nx\n1234");
        key(&mut editor, 3, 0);
        key(&mut editor, 3, 0);
        assert_eq!(editor.cursor, "中文".len());
    }

    #[test]
    fn inserted_combining_mark_does_not_leave_cursor_inside_grapheme() {
        let mut editor = Editor::default();
        editor.restore("ab", 1).unwrap();
        editor.event(0, 10, 0, "\u{301}").unwrap();
        key(&mut editor, 7, 0);
        assert_eq!(editor.text, "b");
    }

    #[test]
    fn invalid_restore_preserves_draft_and_history() {
        let mut editor = Editor::default();
        editor.event(1, 0, 0, "keep").unwrap();
        for (text, offset) in [("e\u{301}", 1), ("中文", 1), ("bad\u{1b}", 0)] {
            assert!(editor.restore(text, offset).is_err());
            assert_eq!(editor.text, "keep");
        }
        key(&mut editor, 12, 0);
        assert_eq!(editor.text, "");
    }

    #[test]
    fn transfer_preserves_selection_history_kill_and_scroll_with_new_style() {
        // SAFETY: the serialized SDK call keeps its library, handle and borrowed buffers live; tests create these values locally.
        unsafe {
            let first = create(0);
            let second = create(1);
            let editor = &mut *first.cast::<Editor>();
            editor.event(1, 0, 0, "a\nb\nc\nd").unwrap();
            key(editor, 19, 0);
            key(editor, 16, 0);
            key(editor, 1, 1);
            editor.scroll = 2;
            assert_eq!(transfer_state(second, first), 0);
            let next = &mut *second.cast::<Editor>();
            assert!(next.numbered);
            assert_eq!(next.selected_text(), "d");
            assert_eq!(next.kill, "d");
            assert_eq!(next.scroll, 2);
            key(next, 12, 0);
            assert_eq!(next.text, "a\nb\nc\n");
            key(next, 13, 0);
            assert_eq!(next.text, "a\nb\nc\nd");
            assert_eq!(editor.text, "a\nb\nc\nd");
            destroy(first);
            destroy(second);
        }
    }

    #[test]
    fn invalid_abi_input_is_rejected() {
        // SAFETY: the serialized SDK call keeps its library, handle and borrowed buffers live; tests create these values locally.
        unsafe {
            let state = create(0);
            assert_eq!(event(state, 1, 0, 0, [0xff].as_ptr(), 1), EVENT_ERROR);
            assert_eq!(restore(state, std::ptr::null(), 1, 0), -1);
            assert_eq!(transfer_state(state, std::ptr::null_mut()), -1);
            destroy(state);
        }
    }

    #[derive(Debug)]
    struct DrawnCell {
        x: u16,
        y: u16,
        flags: u32,
        text: String,
        bg: u32,
    }

    extern "C" fn collect(ctx: *mut c_void, cell: *const Cell) {
        // SAFETY: the serialized SDK call keeps its library, handle and borrowed buffers live; tests create these values locally.
        unsafe {
            let output = &mut *ctx.cast::<Vec<DrawnCell>>();
            let cell = &*cell;
            let text =
                String::from_utf8_lossy(std::slice::from_raw_parts(cell.text, cell.text_len))
                    .into_owned();
            output.push(DrawnCell {
                x: cell.x,
                y: cell.y,
                flags: cell.flags,
                text,
                bg: cell.bg,
            });
        }
    }

    fn draw(editor: &mut Editor, width: u16, height: u16, mode: u32) -> Vec<DrawnCell> {
        let mut output = Vec::<DrawnCell>::new();
        editor.render(
            width,
            height,
            mode,
            true,
            collect,
            (&mut output as *mut Vec<DrawnCell>).cast(),
        );
        output
    }

    #[test]
    fn wrapped_cursor_selection_and_resize_stay_inside_viewport() {
        let mut editor = Editor::default();
        editor.event(1, 0, 0, "中文\ne\u{301}👩‍💻tail").unwrap();
        key(&mut editor, 1, 1);
        for (width, height) in [(1, 1), (4, 2), (8, 3), (0, 0)] {
            let output = draw(&mut editor, width, height, 0);
            assert!(output.iter().all(|c| c.x < width && c.y < height));
            if width > 0 {
                assert!(output.iter().any(|c| c.flags == CELL_CURSOR));
            }
        }
        let output = draw(&mut editor, 20, 4, 0);
        assert!(
            output
                .iter()
                .any(|c| c.flags & CELL_REVERSE != 0 && c.text == "l")
        );
        assert!(output.iter().all(|c| c.bg == COLOR_DEFAULT));
    }

    #[test]
    fn scroll_tracks_cursor_in_both_directions_without_jumping_on_small_moves() {
        let mut editor = Editor::default();
        editor.event(1, 0, 0, "a\nb\nc\nd\ne").unwrap();
        draw(&mut editor, 10, 2, 0);
        assert_eq!(editor.scroll, 3);
        key(&mut editor, 3, 0);
        draw(&mut editor, 10, 2, 0);
        assert_eq!(editor.scroll, 3);
        key(&mut editor, 5, 2);
        draw(&mut editor, 10, 2, 0);
        assert_eq!(editor.scroll, 0);
    }

    #[test]
    fn exact_width_newline_does_not_add_blank_row_and_code_has_no_cursor() {
        let mut editor = Editor::default();
        editor.event(1, 0, 0, "中文\nX").unwrap();
        let output = draw(&mut editor, 4, 3, 0);
        assert!(output.iter().any(|c| c.x == 0 && c.y == 1 && c.text == "X"));
        let output = draw(&mut editor, 20, 3, 2);
        assert!(!output.iter().any(|c| c.flags == CELL_CURSOR));
        assert!(output.iter().any(|c| c.x == 4 && c.y == 1 && c.text == "X"));
    }

    #[test]
    fn pointer_after_exact_width_newline_targets_next_logical_line() {
        let mut editor = Editor::default();
        editor.restore("中文\nX", 0).unwrap();
        draw(&mut editor, 4, 3, 0);
        editor.event(2, 1 << 16, 0, "").unwrap();
        assert_eq!(editor.cursor, "中文\n".len());
        editor.event(2, (1 << 16) | 3, 1, "").unwrap();
        assert_eq!(editor.selected_text(), "X");
    }

    #[test]
    fn pointer_after_resize_uses_new_wrapping() {
        let mut editor = Editor::default();
        editor.restore("ab中👩‍💻tail", 0).unwrap();
        draw(&mut editor, 4, 4, 0);
        editor.event(2, 1 << 16, 0, "").unwrap();
        assert_eq!(editor.cursor, "ab中".len());
        draw(&mut editor, 8, 4, 0);
        editor.event(2, 1 << 16, 0, "").unwrap();
        assert_eq!(editor.cursor, "ab中👩‍💻ta".len());
    }

    #[test]
    fn mouse_selection_copy_and_paste_match_painted_unicode_for_all_cell_endpoints() {
        for source in ["中文X", "e\u{301}👩‍💻中文", "🇨🇳a👨‍👩‍👧‍👦"]
        {
            for start in 0..=source.width() {
                for end in 0..=source.width() {
                    let mut editor = Editor::default();
                    editor.restore(source, 0).unwrap();
                    draw(&mut editor, 24, 3, 0);
                    editor.event(2, start as u32, 0, "").unwrap();
                    editor.event(2, end as u32, 1, "").unwrap();
                    let selected = editor.selected_text().to_owned();
                    let painted: String = draw(&mut editor, 24, 3, 0)
                        .iter()
                        .filter(|c| c.flags & CELL_REVERSE != 0)
                        .map(|c| c.text.as_str())
                        .collect();
                    assert_eq!(painted, selected, "{source:?} cells {start}..{end}");
                    let mut destination = Editor::default();
                    destination.event(1, 0, 0, &selected).unwrap();
                    assert_eq!(destination.text, selected);
                }
            }
        }
    }
    #[test]
    fn selection_retains_unicode_and_hard_newlines_after_resize_and_undo() {
        let source = "甲乙丙丁戊己\ne\u{301}👩‍💻 tail";
        let mut editor = Editor::default();
        editor.restore(source, 0).unwrap();
        draw(&mut editor, 6, 8, 0);
        editor.event(2, 0, 0, "").unwrap();
        editor.event(2, (7 << 16) | 5, 1, "").unwrap();
        assert_eq!(editor.selected_text(), source);
        for width in [8, 24, 6] {
            let painted: String = draw(&mut editor, width, 8, 0)
                .iter()
                .filter(|c| c.flags & CELL_REVERSE != 0)
                .map(|c| c.text.as_str())
                .collect();
            assert_eq!(painted, source.replace('\n', ""));
            assert_eq!(editor.selected_text(), source);
        }
        editor.event(1, 0, 0, "替换").unwrap();
        key(&mut editor, 12, 0);
        assert_eq!(editor.selected_text(), source);
    }

    #[test]
    fn pointer_is_rejected_before_render_and_after_viewport_collapses() {
        let mut editor = Editor::default();
        editor.restore("draft", 0).unwrap();
        assert!(editor.event(2, 0, 0, "").is_err());
        draw(&mut editor, 8, 2, 0);
        draw(&mut editor, 0, 0, 0);
        assert!(editor.event(2, 0, 0, "").is_err());
    }
}
