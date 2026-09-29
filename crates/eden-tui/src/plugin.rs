use eden_ui_sdk::{EVENT_ERROR, UiApi};
use libloading::Library;
use ratatui::{buffer::Buffer, layout::Rect};
use std::{ffi::c_void, path::Path, ptr::NonNull};

use crate::extensions::{Surface, receive_cell, validate_table};

pub struct Editor {
    api: NonNull<UiApi>,
    state: *mut c_void,
    pub style: u32,
    _code: Library,
}
impl Editor {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        // SAFETY: the trusted SDK call is serialized; the library, handle and borrowed buffers remain alive until it returns.
        unsafe {
            let code = Library::new(path)?;
            let entry: libloading::Symbol<unsafe extern "C" fn() -> *const UiApi> =
                code.get(b"eden_ui_v1\0")?;
            let api = validate_table(entry())?;
            let state = (api.as_ref().create)(0);
            if state.is_null() {
                return Err("UI create failed".into());
            }
            Ok(Self {
                api,
                state,
                style: 0,
                _code: code,
            })
        }
    }
    fn api(&self) -> &UiApi {
        // SAFETY: load validated the table; _code is alive for this borrow.
        unsafe { self.api.as_ref() }
    }
    pub fn text(&self) -> String {
        self.collect_text(false)
    }
    pub fn selected_text(&self) -> String {
        self.collect_text(true)
    }
    fn collect_text(&self, selected: bool) -> String {
        extern "C" fn collect(ctx: *mut c_void, data: *const u8, len: usize) {
            if !data.is_null() && len <= 4 * 1024 * 1024 {
                // SAFETY: the trusted SDK call is serialized; the library, handle and borrowed buffers remain alive until it returns.
                unsafe {
                    let output = &mut *ctx.cast::<String>();
                    let bytes = std::slice::from_raw_parts(data, len);
                    if let Ok(s) = std::str::from_utf8(bytes) {
                        output.push_str(s)
                    }
                }
            }
        }
        let mut s = String::new();
        let callback = if selected {
            self.api().selected_text
        } else {
            self.api().snapshot
        };
        // SAFETY: the trusted SDK call is serialized; the library, handle and borrowed buffers remain alive until it returns.
        unsafe { callback(self.state, collect, (&mut s as *mut String).cast()) };
        s
    }
    pub fn cursor(&self) -> usize {
        // SAFETY: the trusted SDK call is serialized; the library, handle and borrowed buffers remain alive until it returns.
        unsafe { (self.api().cursor)(self.state) }
    }
    pub fn restore(&mut self, text: &str, cursor: usize) -> Result<(), String> {
        // SAFETY: the trusted SDK call is serialized; the library, handle and borrowed buffers remain alive until it returns.
        if unsafe { (self.api().restore)(self.state, text.as_ptr(), text.len(), cursor) } != 0 {
            Err("editor restore failed".into())
        } else {
            Ok(())
        }
    }
    pub fn event(&mut self, kind: u32, key: u32, mods: u32, text: &str) -> Result<(), String> {
        // SAFETY: the trusted SDK call is serialized; the library, handle and borrowed buffers remain alive until it returns.
        if unsafe { (self.api().event)(self.state, kind, key, mods, text.as_ptr(), text.len()) }
            == EVENT_ERROR
        {
            Err("editor rejected input".into())
        } else {
            Ok(())
        }
    }
    pub fn replace_range(
        &mut self,
        range: std::ops::Range<usize>,
        text: &str,
    ) -> Result<(), String> {
        let start = u32::try_from(range.start).map_err(|_| "replacement range too large")?;
        let end = u32::try_from(range.end).map_err(|_| "replacement range too large")?;
        self.event(3, start, end, text)
    }
    pub fn mouse(&mut self, column: u16, row: u16, extend: bool) -> Result<(), String> {
        self.event(
            2,
            (u32::from(row) << 16) | u32::from(column),
            u32::from(extend),
            "",
        )
    }
    pub fn switch(&mut self) -> Result<(), String> {
        let style = 1 - self.style;
        // SAFETY: the trusted SDK call is serialized; the library, handle and borrowed buffers remain alive until it returns.
        let next = unsafe { (self.api().create)(style) };
        if next.is_null() {
            return Err("editor create failed".into());
        }
        // SAFETY: the trusted SDK call is serialized; the library, handle and borrowed buffers remain alive until it returns.
        if unsafe { (self.api().transfer_state)(next, self.state) } != 0 {
            // SAFETY: the trusted SDK call is serialized; the library, handle and borrowed buffers remain alive until it returns.
            unsafe { (self.api().destroy)(next) };
            return Err("draft transfer failed".into());
        }
        // SAFETY: the trusted SDK call is serialized; the library, handle and borrowed buffers remain alive until it returns.
        unsafe { (self.api().destroy)(self.state) };
        self.state = next;
        self.style = style;
        Ok(())
    }
    pub fn draw(
        &mut self,
        buf: &mut Buffer,
        area: Rect,
        mode: u32,
        dark: bool,
        mono: bool,
    ) -> Option<(u16, u16)> {
        let mut surface = Surface {
            buffer: buf,
            area,
            cursor: None,
            mono,
        };
        // SAFETY: the trusted SDK call is serialized; the library, handle and borrowed buffers remain alive until it returns.
        unsafe {
            (self.api().render)(
                self.state,
                area.width,
                area.height,
                mode,
                u32::from(dark),
                receive_cell,
                (&mut surface as *mut Surface<'_>).cast(),
            )
        };
        surface.cursor
    }
}
impl Drop for Editor {
    fn drop(&mut self) {
        // SAFETY: the trusted SDK call is serialized; the library, handle and borrowed buffers remain alive until it returns.
        unsafe { (self.api().destroy)(self.state) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    fn native_editor() -> Editor {
        let executable = std::env::current_exe().unwrap();
        let build_directory = executable.parent().unwrap().parent().unwrap();
        let path = build_directory.join(format!(
            "{}eden_terminal_editor{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX
        ));
        Editor::load(&path).expect("build eden-terminal-editor cdylib before running app tests")
    }

    #[test]
    fn dynamic_editor_switch_preserves_selection_undo_redo_and_kill_buffer() {
        let mut editor = native_editor();
        editor.event(1, 0, 0, "中👩‍💻\ne\u{301}").unwrap();
        editor.event(0, 19, 0, "").unwrap();
        editor.event(0, 16, 0, "").unwrap();
        editor.event(0, 1, 1, "").unwrap();
        let cursor = editor.cursor();
        editor.switch().unwrap();
        assert_eq!(editor.style, 1);
        assert_eq!(editor.cursor(), cursor);
        assert_eq!(editor.selected_text(), "e\u{301}");
        editor.event(0, 12, 0, "").unwrap();
        assert_eq!(editor.text(), "中👩‍💻\n");
        editor.event(0, 13, 0, "").unwrap();
        assert_eq!(editor.text(), "中👩‍💻\ne\u{301}");
        editor.event(0, 2, 0, "").unwrap();
        editor.event(0, 16, 0, "").unwrap();
        assert_eq!(editor.text(), "中👩‍💻\ne\u{301}e\u{301}");
    }

    #[test]
    fn native_render_respects_host_offset_themes_and_mono_selection() {
        let mut editor = native_editor();
        editor.restore("中👩‍💻", "中👩‍💻".len()).unwrap();
        editor.event(0, 1, 1, "").unwrap();
        let area = Rect::new(3, 2, 12, 3);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 8));
        let cursor = editor.draw(&mut buffer, area, 0, true, false);
        assert_eq!(cursor, Some((5, 2)));
        assert_eq!(buffer[(3, 2)].bg, Color::Reset);
        assert_eq!(buffer[(0, 0)].symbol(), " ");
        editor.draw(&mut buffer, area, 0, false, false);
        assert_eq!(buffer[(3, 2)].bg, Color::Reset);
        editor.draw(&mut buffer, area, 0, true, true);
        assert_eq!(buffer[(5, 2)].fg, Color::Reset);
        assert!(buffer[(5, 2)].modifier.contains(Modifier::REVERSED));
        assert_eq!(editor.draw(&mut buffer, area, 2, true, false), None);
    }

    #[test]
    fn native_pointer_drag_selects_whole_graphemes_across_wrapped_rows() {
        let mut editor = native_editor();
        editor
            .restore("中👩‍💻e\u{301} abc\n0123456789\nend", 0)
            .unwrap();
        let area = Rect::new(5, 3, 8, 3);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 10));
        editor.draw(&mut buffer, area, 0, true, false);
        editor.mouse(3, 0, false).unwrap();
        assert_eq!(editor.cursor(), "中".len());
        editor.mouse(0, 1, true).unwrap();
        assert_eq!(editor.selected_text(), "👩‍💻e\u{301} ab");
        editor.event(1, 0, 0, "new").unwrap();
        assert_eq!(editor.text(), "中newc\n0123456789\nend");
        assert!(
            editor.mouse(0, 0, false).is_err(),
            "edits invalidate pointer mapping until redraw"
        );
    }

    #[test]
    fn native_pointer_uses_visible_scroll_and_numbered_gutter() {
        let mut editor = native_editor();
        let draft = "zero\n中👩‍💻xy\nlast";
        editor.restore(draft, draft.len()).unwrap();
        editor.switch().unwrap();
        let area = Rect::new(0, 0, 10, 2);
        let mut buffer = Buffer::empty(area);
        editor.draw(&mut buffer, area, 0, true, false);
        editor.draw(&mut buffer, area, 1, true, false);
        editor.mouse(1, 0, false).unwrap();
        assert_eq!(editor.cursor(), "zero\n".len());
        editor.mouse(8, 0, true).unwrap();
        assert_eq!(editor.selected_text(), "中👩‍💻");
        editor.draw(&mut buffer, area, 0, true, false);
        assert!(buffer[(4, 0)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buffer[(4, 0)].bg, Color::Reset);
    }

    #[test]
    fn native_render_uses_terminal_background_in_every_mode_and_theme() {
        let mut editor = native_editor();
        editor.restore("let value = 42;", 0).unwrap();
        for dark in [false, true] {
            for mode in 0..=2 {
                let area = Rect::new(0, 0, 24, 8);
                let mut buffer = Buffer::empty(area);
                for cell in &mut buffer.content {
                    cell.set_bg(Color::Magenta);
                }
                editor.draw(&mut buffer, area, mode, dark, false);
                assert!(buffer.content.iter().all(|cell| cell.bg == Color::Reset));
            }
        }
    }
}
