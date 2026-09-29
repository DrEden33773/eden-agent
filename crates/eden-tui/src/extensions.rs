//! Safe owners for trusted native UI roles. No terminal or framework value crosses C.
use std::{ffi::c_void, path::Path, ptr::NonNull};

use eden_ui_sdk::{
    ABI_VERSION, ApiHeader, CELL_BOLD, CELL_CURSOR, CELL_REVERSE, CELL_UNDERLINE, COLOR_DEFAULT,
    Cell, RendererApi, TerminalFrontendApi, ThemeApi,
};
use libloading::Library;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};

pub(crate) type LoadError = Box<dyn std::error::Error>;

/// Validate the common prefix without forming a reference to a possibly shorter table.
///
/// # Safety
/// `table` must be a live, aligned pointer returned by a trusted library entry point,
/// with at least an initialized ApiHeader. Its library must remain loaded.
pub(crate) unsafe fn validate_table<T>(table: *const T) -> Result<NonNull<T>, LoadError> {
    let table = NonNull::new(table.cast_mut()).ok_or("null UI table")?;
    // SAFETY: the entry point guarantees the common header; the full table is not read yet.
    let header = unsafe { table.cast::<ApiHeader>().as_ref() };
    if header.abi != ABI_VERSION || header.table_size < std::mem::size_of::<T>() as u32 {
        return Err("incompatible UI ABI".into());
    }
    Ok(table)
}

pub(crate) fn color(rgb: u32, mono: bool) -> Color {
    if mono || rgb == COLOR_DEFAULT || rgb > 0x00ff_ffff {
        Color::Reset
    } else {
        Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
    }
}

/// A callback context borrowed for one render; clipping also covers caller-supplied rectangles.
pub(crate) struct Surface<'a> {
    pub(crate) buffer: &'a mut Buffer,
    pub(crate) area: Rect,
    pub(crate) cursor: Option<(u16, u16)>,
    pub(crate) mono: bool,
}

pub(crate) extern "C" fn receive_cell(ctx: *mut c_void, cell: *const Cell) {
    if cell.is_null() || ctx.is_null() {
        return;
    }
    // SAFETY: callers pass a live exclusive Surface for this synchronous callback, and
    // the trusted plugin borrows a valid Cell plus text only until this callback returns.
    let (surface, cell) = unsafe { (&mut *ctx.cast::<Surface<'_>>(), &*cell) };
    if cell.x >= surface.area.width || cell.y >= surface.area.height {
        return;
    }
    let Some(x) = surface.area.x.checked_add(cell.x) else {
        return;
    };
    let Some(y) = surface.area.y.checked_add(cell.y) else {
        return;
    };
    if !surface.buffer.area.contains((x, y).into()) {
        return;
    }
    if cell.flags & CELL_CURSOR != 0 {
        surface.cursor = Some((x, y));
        return;
    }
    if cell.text_len == 0 || cell.text.is_null() || cell.text_len > 4096 {
        return;
    }
    // SAFETY: the SDK callback contract keeps these bytes readable for text_len.
    let bytes = unsafe { std::slice::from_raw_parts(cell.text, cell.text_len) };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    if text.chars().any(char::is_control) {
        return;
    }
    let mut modifiers = Modifier::empty();
    for (flag, modifier) in [
        (CELL_BOLD, Modifier::BOLD),
        (CELL_REVERSE, Modifier::REVERSED),
        (CELL_UNDERLINE, Modifier::UNDERLINED),
    ] {
        if cell.flags & flag != 0 {
            modifiers.insert(modifier);
        }
    }
    let style = Style::default()
        .remove_modifier(Modifier::all())
        .fg(color(cell.fg, surface.mono))
        .bg(color(cell.bg, surface.mono))
        .add_modifier(modifiers);
    let remaining = (surface.area.width - cell.x).min(surface.buffer.area.right() - x);
    surface
        .buffer
        .set_stringn(x, y, text, usize::from(remaining), style);
}

/// Keeps a stateless theme table and its code alive on the calling thread.
pub(crate) struct Theme {
    api: NonNull<ThemeApi>,
    _code: Library,
}
impl Theme {
    pub(crate) fn load(path: &Path) -> Result<Self, LoadError> {
        // SAFETY: loading an explicitly selected trusted native library executes its code.
        // The symbol follows the SDK; validate its prefix before reading its full table.
        unsafe {
            let code = Library::new(path)?;
            let entry =
                code.get::<unsafe extern "C" fn() -> *const ThemeApi>(b"eden_theme_v1\0")?;
            let api = validate_table(entry())?;
            Ok(Self { api, _code: code })
        }
    }
    pub(crate) fn resolve(&self, dark: bool, token: u32) -> u32 {
        // SAFETY: the library remains owned here; this !Send/!Sync owner serializes calls.
        unsafe { (self.api.as_ref().resolve)(u32::from(dark), token) }
    }
}

/// Releases renderer state through its creating library before unloading that code.
pub(crate) struct Renderer {
    api: NonNull<RendererApi>,
    state: NonNull<c_void>,
    _code: Library,
}
impl Renderer {
    pub(crate) fn load(path: &Path) -> Result<Self, LoadError> {
        // SAFETY: trusted native code and ABI entry point, retained through handle destruction.
        unsafe {
            let code = Library::new(path)?;
            let entry =
                code.get::<unsafe extern "C" fn() -> *const RendererApi>(b"eden_renderer_v1\0")?;
            let api = validate_table(entry())?;
            let state = NonNull::new((api.as_ref().create)()).ok_or("renderer create failed")?;
            Ok(Self {
                api,
                state,
                _code: code,
            })
        }
    }
    /// Returns an error without changing the destination, so built-in drawing can fall back.
    pub(crate) fn render(
        &mut self,
        payload: &serde_json::Value,
        buffer: &mut Buffer,
        area: Rect,
        mono: bool,
    ) -> Result<Option<(u16, u16)>, LoadError> {
        let json = serde_json::to_vec(payload)?;
        let mut staged = buffer.clone();
        let mut surface = Surface {
            buffer: &mut staged,
            area,
            cursor: None,
            mono,
        };
        // SAFETY: input and Surface live through the synchronous call; state belongs to
        // this library and callbacks copy each cell without retaining plugin pointers.
        let status = unsafe {
            (self.api.as_ref().render)(
                self.state.as_ptr(),
                json.as_ptr(),
                json.len(),
                area.width,
                area.height,
                receive_cell,
                (&mut surface as *mut Surface<'_>).cast(),
            )
        };
        let cursor = surface.cursor;
        if status != 0 {
            return Err("renderer rejected frame".into());
        }
        *buffer = staged;
        Ok(cursor)
    }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY: state was created by this table and is consumed once before _code drops.
        unsafe { (self.api.as_ref().destroy)(self.state.as_ptr()) };
    }
}

/// Runs a selected frontend before the built-in frontend acquires the terminal.
pub(crate) struct Frontend {
    api: NonNull<TerminalFrontendApi>,
    _code: Library,
}
impl Frontend {
    pub(crate) fn load(path: &Path) -> Result<Self, LoadError> {
        // SAFETY: trusted library selected by the user; its code outlives the validated table.
        unsafe {
            let code = Library::new(path)?;
            let entry = code.get::<unsafe extern "C" fn() -> *const TerminalFrontendApi>(
                b"eden_terminal_frontend_v1\0",
            )?;
            let api = validate_table(entry())?;
            Ok(Self { api, _code: code })
        }
    }
    /// Configuration bytes live for the entire call; the plugin owns its terminal cleanup.
    pub(crate) fn run(&mut self, configuration: &serde_json::Value) -> Result<i32, LoadError> {
        let json = serde_json::to_vec(configuration)?;
        // SAFETY: the plugin borrows serialized configuration until run returns, and the
        // library stays loaded until all plugin tasks and terminal resources have stopped.
        Ok(unsafe { (self.api.as_ref().run)(json.as_ptr(), json.len()) })
    }
}

/// The overlay retains one renderer state and serializes event/render calls on the UI thread.
pub(crate) struct Overlay {
    renderer: Renderer,
    event: unsafe extern "C" fn(
        *mut c_void,
        *const u8,
        usize,
        eden_ui_sdk::ByteSink,
        *mut c_void,
    ) -> i32,
}
impl Overlay {
    pub(crate) fn load(path: &Path) -> Result<Self, LoadError> {
        // SAFETY: the explicitly selected trusted table begins with RendererApi; its
        // header is checked against the full OverlayApi before the event pointer is read.
        unsafe {
            let code = Library::new(path)?;
            let entry = code.get::<unsafe extern "C" fn() -> *const eden_ui_sdk::OverlayApi>(
                b"eden_overlay_v1\0",
            )?;
            let api = validate_table(entry())?;
            let event = api.as_ref().event;
            let state =
                NonNull::new((api.as_ref().renderer.create)()).ok_or("overlay create failed")?;
            Ok(Self {
                renderer: Renderer {
                    api: api.cast(),
                    state,
                    _code: code,
                },
                event,
            })
        }
    }
    pub(crate) fn render(
        &mut self,
        payload: &serde_json::Value,
        buffer: &mut Buffer,
        area: Rect,
        mono: bool,
    ) -> Result<Option<(u16, u16)>, LoadError> {
        self.renderer.render(payload, buffer, area, mono)
    }
    pub(crate) fn key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Option<crossterm::event::KeyCode> {
        let input = serde_json::to_vec(&serde_json::json!({
            "key": format!("{:?}", key.code),
            "modifiers": key.modifiers.bits(),
        }))
        .ok()?;
        let mut output = Vec::<u8>::new();
        extern "C" fn receive(ctx: *mut c_void, bytes: *const u8, len: usize) {
            if ctx.is_null() || bytes.is_null() || len > 128 {
                return;
            }
            // SAFETY: the SDK borrows initialized bytes and the exclusive Vec for this call.
            let out = unsafe { &mut *ctx.cast::<Vec<u8>>() };
            if out.len() + len <= 128 {
                // SAFETY: the callback lends len initialized bytes until this call returns.
                out.extend_from_slice(unsafe { std::slice::from_raw_parts(bytes, len) });
            }
        }
        // SAFETY: input and callback context remain live; this owner prevents reentry and
        // keeps both the state and its creating library alive throughout the callback.
        let status = unsafe {
            (self.event)(
                self.renderer.state.as_ptr(),
                input.as_ptr(),
                input.len(),
                receive,
                (&mut output as *mut Vec<u8>).cast(),
            )
        };
        if status != 0 {
            return None;
        }
        use crossterm::event::KeyCode;
        match serde_json::from_slice::<String>(&output).ok()?.as_str() {
            "up" => Some(KeyCode::Up),
            "down" => Some(KeyCode::Down),
            "accept" => Some(KeyCode::Enter),
            "close" => Some(KeyCode::Esc),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_validation_rejects_short_or_wrong_version_tables() {
        let short = ApiHeader {
            abi: ABI_VERSION,
            table_size: std::mem::size_of::<ApiHeader>() as u32,
        };
        // SAFETY: the header is live and aligned; validation must not read a full table.
        assert!(unsafe { validate_table((&raw const short).cast::<RendererApi>()) }.is_err());
        let wrong = ApiHeader {
            abi: ABI_VERSION + 1,
            table_size: std::mem::size_of::<RendererApi>() as u32,
        };
        // SAFETY: only the live prefix is accessed when the version is rejected.
        assert!(unsafe { validate_table((&raw const wrong).cast::<RendererApi>()) }.is_err());
    }

    #[test]
    fn cell_callback_clips_to_buffer_and_rejects_terminal_controls() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 2));
        let mut surface = Surface {
            buffer: &mut buffer,
            area: Rect::new(2, 1, 50, 50),
            cursor: None,
            mono: false,
        };
        let mut cell = Cell {
            x: 0,
            y: 0,
            fg: COLOR_DEFAULT,
            bg: COLOR_DEFAULT,
            flags: 0,
            text: b"abc".as_ptr(),
            text_len: 3,
        };
        let ctx = (&mut surface as *mut Surface<'_>).cast();
        receive_cell(ctx, &cell);
        cell.x = 1;
        receive_cell(ctx, &cell);
        cell.x = 0;
        cell.text = b"\x1b".as_ptr();
        cell.text_len = 1;
        receive_cell(ctx, &cell);
        assert_eq!(surface.buffer[(2, 1)].symbol(), "a");
        assert_eq!(surface.buffer[(2, 1)].bg, Color::Reset);
    }

    #[test]
    #[ignore = "build the independent author first and set EDEN_TEST_UI_AUTHOR to its dynamic \
                library"]
    fn independently_built_roles_load_and_renderer_failure_preserves_frame() {
        let path = std::env::var_os("EDEN_TEST_UI_AUTHOR").expect("author library path");
        let mut editor = crate::plugin::Editor::load(Path::new(&path)).unwrap();
        editor.event(1, 0, 0, "external author").unwrap();
        assert_eq!(editor.text(), "external author");
        let theme = Theme::load(Path::new(&path)).unwrap();
        assert_eq!(
            theme.resolve(true, eden_ui_sdk::theme_token::ACCENT),
            0x12ab34
        );
        let mut renderer = Renderer::load(Path::new(&path)).unwrap();
        let area = Rect::new(2, 1, 20, 2);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 4));
        renderer
            .render(
                &serde_json::json!({ "messages": [{ "body": "sample" }] }),
                &mut buffer,
                area,
                false,
            )
            .unwrap();
        assert_eq!(buffer[(2, 1)].symbol(), "A");
        assert_eq!(buffer[(2, 1)].fg, Color::Rgb(0x12, 0xab, 0x34));
        assert_eq!(buffer[(2, 1)].bg, Color::Reset);
        buffer.reset();
        let before = buffer.clone();
        assert!(
            renderer
                .render(
                    &serde_json::json!({ "messages": [], "fail_after_draw": true }),
                    &mut buffer,
                    area,
                    false
                )
                .is_err()
        );
        assert_eq!(buffer, before);
        let mut frontend = Frontend::load(Path::new(&path)).unwrap();
        assert_eq!(frontend.run(&serde_json::json!({})).unwrap(), 2);
    }
}
