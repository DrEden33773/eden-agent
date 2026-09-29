//! Behaviour of the tables `export_ui!` generates: the layout golden values, the headers
//! of all five entry points, every slot, and the failure value each slot must answer when
//! the role panics or refuses.
//!
//! This is a separate crate from the SDK, so the macro expands exactly the way an
//! independent author's crate expands it.
use eden_ui_sdk::{
    ABI_VERSION, CELL_BOLD, CELL_CURSOR, COLOR_DEFAULT, Cell, EVENT_ERROR, OverlayApi, RendererApi,
    TerminalFrontendApi, ThemeApi, UiApi, author, theme_token,
};
use std::{
    cell::Cell as Slot,
    ffi::c_void,
    mem::{offset_of, size_of},
};

// The roles under test report an injected panic through a thread-local slot, so a test
// can drive one slot at a time even while other tests run in parallel threads.
thread_local! {
    static PANIC_AT: Slot<u32> = const { Slot::new(NO_PANIC) };
    static CALLS: Slot<u32> = const { Slot::new(0) };
}

const NO_PANIC: u32 = 0;
const PANIC_CREATE: u32 = 1;
const PANIC_DESTROY: u32 = 2;
const PANIC_EVENT: u32 = 3;
const PANIC_RENDER: u32 = 4;
const PANIC_SNAPSHOT: u32 = 5;
const PANIC_CURSOR: u32 = 6;
const PANIC_RESTORE: u32 = 7;
const PANIC_SELECTED_TEXT: u32 = 8;
const PANIC_TRANSFER: u32 = 9;
const PANIC_RESOLVE: u32 = 10;
const PANIC_RUN: u32 = 11;
const PANIC_OVERLAY_EVENT: u32 = 12;

fn panic_at(slot: u32) {
    PANIC_AT.with(|value| value.set(slot));
}

fn clear_panics() {
    PANIC_AT.with(|value| value.set(NO_PANIC));
}

fn panic_if(slot: u32) {
    PANIC_AT.with(|value| {
        if value.get() == slot {
            panic!("injected panic in slot {slot}");
        }
    });
}

/// Count role calls so a test can prove an invalid call never reached the author.
fn enter(slot: u32) {
    panic_if(slot);
    CALLS.with(|value| value.set(value.get() + 1));
}

fn calls() -> u32 {
    CALLS.with(Slot::get)
}

fn reset_calls() {
    CALLS.with(|value| value.set(0));
}

#[derive(Debug, PartialEq, Eq)]
struct DrawnCell {
    x: u16,
    y: u16,
    fg: u32,
    bg: u32,
    flags: u32,
    text: String,
}

extern "C" fn collect_cell(ctx: *mut c_void, cell: *const Cell) {
    if ctx.is_null() || cell.is_null() {
        return;
    }
    // SAFETY: the tests pass a live Vec as the callback context and the host lends each
    // cell for this call only.
    let (output, cell) = unsafe { (&mut *ctx.cast::<Vec<DrawnCell>>(), &*cell) };
    if cell.flags & CELL_CURSOR != 0 {
        output.push(DrawnCell {
            x: cell.x,
            y: cell.y,
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags,
            text: String::new(),
        });
        return;
    }
    let text = if cell.text_len == 0 {
        ""
    } else {
        // SAFETY: the host keeps the cell text readable until this callback returns.
        std::str::from_utf8(unsafe { std::slice::from_raw_parts(cell.text, cell.text_len) })
            .expect("the roles under test paint UTF-8")
    };
    output.push(DrawnCell {
        x: cell.x,
        y: cell.y,
        fg: cell.fg,
        bg: cell.bg,
        flags: cell.flags,
        text: text.to_owned(),
    });
}

extern "C" fn collect_bytes(ctx: *mut c_void, bytes: *const u8, len: usize) {
    if ctx.is_null() || bytes.is_null() || len == 0 {
        return;
    }
    // SAFETY: the tests pass a live Vec as the callback context and the host lends the
    // bytes for this call only.
    unsafe { (*ctx.cast::<Vec<u8>>()).extend_from_slice(std::slice::from_raw_parts(bytes, len)) };
}

mod draft {
    use super::*;

    const MAX_EVENT_TEXT: usize = 8;

    #[derive(Default)]
    pub struct Draft {
        text: String,
        cursor: usize,
        selection: usize,
        numbered: bool,
    }

    impl author::Editor for Draft {
        fn create(style: u32) -> Option<Self> {
            enter(PANIC_CREATE);
            (style <= 1).then(|| Self {
                numbered: style == 1,
                ..Self::default()
            })
        }

        fn event(
            &mut self,
            kind: u32,
            key: u32,
            modifiers: u32,
            text: &str,
        ) -> Result<(), author::Rejected> {
            enter(PANIC_EVENT);
            if text.len() > MAX_EVENT_TEXT {
                return Err(author::Rejected);
            }
            match kind {
                1 | 3 => {
                    self.text.push_str(text);
                    self.cursor = self.text.len();
                }
                2 => {
                    let position = (key as usize).min(self.text.len());
                    if modifiers & 1 == 1 {
                        self.selection = position;
                    } else {
                        self.cursor = position;
                    }
                }
                _ if key == 7 => {
                    self.text.pop();
                    self.cursor = self.text.len();
                }
                _ => {}
            }
            Ok(())
        }

        fn render(&mut self, frame: &mut author::Frame<'_>) {
            enter(PANIC_RENDER);
            frame.set(
                0,
                0,
                &self.text,
                author::CellStyle {
                    fg: 0x11_2233,
                    bg: if frame.dark() { 0 } else { COLOR_DEFAULT },
                    flags: if self.numbered { CELL_BOLD } else { 0 },
                },
            );
            if frame.mode() == 1 {
                frame.set(1, 0, "menu", author::CellStyle::default());
            }
            if frame.width() > 0 && frame.height() > 0 {
                frame.cursor(0, 0);
            }
        }

        fn snapshot(&self) -> &str {
            enter(PANIC_SNAPSHOT);
            &self.text
        }

        fn cursor(&self) -> usize {
            enter(PANIC_CURSOR);
            self.cursor
        }

        fn restore(&mut self, text: &str, cursor: usize) -> Result<(), author::Rejected> {
            enter(PANIC_RESTORE);
            if cursor > text.len() {
                return Err(author::Rejected);
            }
            self.text = text.to_owned();
            self.cursor = cursor;
            self.selection = 0;
            Ok(())
        }

        fn selected_text(&self) -> &str {
            enter(PANIC_SELECTED_TEXT);
            &self.text[..self.selection]
        }

        fn transfer_state(&mut self, source: &Self) {
            enter(PANIC_TRANSFER);
            let numbered = self.numbered;
            self.text = source.text.clone();
            self.cursor = source.cursor;
            self.selection = source.selection;
            self.numbered = numbered;
        }
    }

    eden_ui_sdk::export_ui!(editor: Draft);
}

mod painter {
    use super::*;

    pub struct Painter;

    impl author::Renderer for Painter {
        fn create() -> Option<Self> {
            enter(PANIC_CREATE);
            Some(Self)
        }

        fn render(
            &mut self,
            payload: &[u8],
            frame: &mut author::Frame<'_>,
        ) -> Result<(), author::Rejected> {
            enter(PANIC_RENDER);
            if payload == b"reject" {
                return Err(author::Rejected);
            }
            let body = std::str::from_utf8(payload).unwrap_or("binary");
            frame.set(0, 0, body, author::CellStyle::default());
            // A renderer payload carries its own theme, so the frame must report the
            // neutral value rather than a dark flag it was never given.
            frame.set(
                1,
                0,
                if frame.dark() { "dark" } else { "light" },
                Default::default(),
            );
            Ok(())
        }
    }

    eden_ui_sdk::export_ui!(renderer: Painter);
}

mod palette {
    use super::*;

    pub struct Palette;

    impl author::Theme for Palette {
        fn resolve(dark: bool, token: u32) -> u32 {
            enter(PANIC_RESOLVE);
            match (dark, token) {
                (true, theme_token::ACCENT) => 0x12_ab_34,
                (false, theme_token::ACCENT) => 0xab_12_34,
                _ => COLOR_DEFAULT,
            }
        }
    }

    eden_ui_sdk::export_ui!(theme: Palette);
}

mod shell {
    use super::*;

    pub struct Shell;

    impl author::Frontend for Shell {
        fn run(config: &[u8]) -> i32 {
            enter(PANIC_RUN);
            if config == b"refuse" {
                return 3;
            }
            i32::try_from(config.len()).unwrap_or(4)
        }
    }

    eden_ui_sdk::export_ui!(frontend: Shell);
}

mod modal {
    use super::*;

    pub struct Modal;

    impl author::Overlay for Modal {
        fn create() -> Option<Self> {
            enter(PANIC_CREATE);
            Some(Self)
        }

        fn render(
            &mut self,
            payload: &[u8],
            frame: &mut author::Frame<'_>,
        ) -> Result<(), author::Rejected> {
            enter(PANIC_RENDER);
            if payload == b"reject" {
                return Err(author::Rejected);
            }
            frame.set(0, 0, "modal", author::CellStyle::default());
            Ok(())
        }

        fn event(&mut self, event: &[u8]) -> Result<&str, author::Rejected> {
            enter(PANIC_OVERLAY_EVENT);
            match event {
                b"{\"key\":\"F(9)\"}" => Ok("\"close\""),
                b"{\"key\":\"x\"}" => Ok("\"pass\""),
                _ => Err(author::Rejected),
            }
        }
    }

    eden_ui_sdk::export_ui!(overlay: Modal);
}

/// The ABI is fixed at version 1 and every table's bytes are a contract with the host.
/// These values are read from the `#[repr(C)]` definitions, so the same test passes
/// before and after a role moves onto the macro; a change here is an ABI change.
#[test]
#[cfg(target_pointer_width = "64")]
fn abi_layout_matches_the_documented_golden_values() {
    assert_eq!(size_of::<eden_ui_sdk::ApiHeader>(), 8);
    assert_eq!(offset_of!(eden_ui_sdk::ApiHeader, abi), 0);
    assert_eq!(offset_of!(eden_ui_sdk::ApiHeader, table_size), 4);

    assert_eq!(size_of::<UiApi>(), 80);
    for (offset, field) in [
        (0, offset_of!(UiApi, abi)),
        (4, offset_of!(UiApi, table_size)),
        (8, offset_of!(UiApi, create)),
        (16, offset_of!(UiApi, destroy)),
        (24, offset_of!(UiApi, event)),
        (32, offset_of!(UiApi, render)),
        (40, offset_of!(UiApi, snapshot)),
        (48, offset_of!(UiApi, cursor)),
        (56, offset_of!(UiApi, restore)),
        (64, offset_of!(UiApi, selected_text)),
        (72, offset_of!(UiApi, transfer_state)),
    ] {
        assert_eq!(offset, field);
    }

    assert_eq!(size_of::<RendererApi>(), 32);
    assert_eq!(offset_of!(RendererApi, header), 0);
    assert_eq!(offset_of!(RendererApi, create), 8);
    assert_eq!(offset_of!(RendererApi, destroy), 16);
    assert_eq!(offset_of!(RendererApi, render), 24);

    assert_eq!(size_of::<ThemeApi>(), 16);
    assert_eq!(offset_of!(ThemeApi, header), 0);
    assert_eq!(offset_of!(ThemeApi, resolve), 8);

    assert_eq!(size_of::<TerminalFrontendApi>(), 16);
    assert_eq!(offset_of!(TerminalFrontendApi, header), 0);
    assert_eq!(offset_of!(TerminalFrontendApi, run), 8);

    assert_eq!(size_of::<OverlayApi>(), 40);
    assert_eq!(offset_of!(OverlayApi, renderer), 0);
    assert_eq!(offset_of!(OverlayApi, event), 32);

    assert_eq!(size_of::<Cell>(), 32);
    assert_eq!(offset_of!(Cell, x), 0);
    assert_eq!(offset_of!(Cell, y), 2);
    assert_eq!(offset_of!(Cell, fg), 4);
    assert_eq!(offset_of!(Cell, bg), 8);
    assert_eq!(offset_of!(Cell, flags), 12);
    assert_eq!(offset_of!(Cell, text), 16);
    assert_eq!(offset_of!(Cell, text_len), 24);
}

/// Every entry point must declare version 1 and a table at least as long as the type the
/// host asked for; the overlay's nested prefix is the full `OverlayApi`, not the
/// `RendererApi` its field type suggests.
#[test]
fn exported_tables_declare_version_one_and_their_own_size() {
    // SAFETY: each entry point returns a table that lives as long as this library.
    unsafe {
        let editor = &*draft::eden_ui_v1();
        assert_eq!(editor.abi, ABI_VERSION);
        assert_eq!(editor.table_size, size_of::<UiApi>() as u32);

        let renderer = &*painter::eden_renderer_v1();
        assert_eq!(renderer.header.abi, ABI_VERSION);
        assert_eq!(renderer.header.table_size, size_of::<RendererApi>() as u32);

        let theme = &*palette::eden_theme_v1();
        assert_eq!(theme.header.abi, ABI_VERSION);
        assert_eq!(theme.header.table_size, size_of::<ThemeApi>() as u32);

        let frontend = &*shell::eden_terminal_frontend_v1();
        assert_eq!(frontend.header.abi, ABI_VERSION);
        assert_eq!(
            frontend.header.table_size,
            size_of::<TerminalFrontendApi>() as u32
        );

        let overlay = &*modal::eden_overlay_v1();
        assert_eq!(overlay.renderer.header.abi, ABI_VERSION);
        assert_eq!(
            overlay.renderer.header.table_size,
            size_of::<OverlayApi>() as u32
        );
        assert_ne!(
            overlay.renderer.header.table_size,
            size_of::<RendererApi>() as u32
        );
    }
}

#[test]
fn editor_slots_round_trip_through_the_generated_table() {
    // SAFETY: this test calls one table serially from one thread and destroys every
    // handle it creates.
    unsafe {
        let api = &*draft::eden_ui_v1();
        let source = (api.create)(0);
        assert!(!source.is_null());
        assert_eq!((api.event)(source, 1, 0, 0, b"draft".as_ptr(), 5), 0);
        assert_eq!((api.event)(source, 2, 3, 0, std::ptr::null(), 0), 0);
        assert_eq!((api.cursor)(source), 3);
        assert_eq!((api.event)(source, 2, 2, 1, std::ptr::null(), 0), 0);

        let mut selected = Vec::<u8>::new();
        (api.selected_text)(
            source,
            collect_bytes,
            (&mut selected as *mut Vec<u8>).cast(),
        );
        assert_eq!(selected, b"dr");

        let mut snapshot = Vec::<u8>::new();
        (api.snapshot)(
            source,
            collect_bytes,
            (&mut snapshot as *mut Vec<u8>).cast(),
        );
        assert_eq!(snapshot, b"draft");

        assert_eq!((api.restore)(source, b"whole".as_ptr(), 5, 5), 0);
        assert_eq!((api.cursor)(source), 5);
        assert_eq!((api.restore)(source, b"whole".as_ptr(), 5, 9), -1);

        let destination = (api.create)(1);
        assert!(!destination.is_null());
        assert_eq!((api.transfer_state)(destination, source), 0);
        assert_eq!((api.transfer_state)(destination, destination), 0);
        assert_eq!((api.transfer_state)(std::ptr::null_mut(), source), -1);

        let mut painted = Vec::<DrawnCell>::new();
        (api.render)(
            destination,
            4,
            1,
            0,
            1,
            collect_cell,
            (&mut painted as *mut Vec<DrawnCell>).cast(),
        );
        // The text came from the source, the numbered style stayed with the destination.
        assert!(painted.iter().any(|cell| cell.text == "whole"
            && cell.flags & CELL_BOLD != 0
            && cell.fg == 0x11_2233
            && cell.bg == 0));

        (api.destroy)(destination);
        (api.destroy)(source);
        (api.destroy)(std::ptr::null_mut());
    }
}

#[test]
fn editor_render_reaches_the_host_cell_sink_and_ignores_unknown_modes() {
    // SAFETY: this test calls one table serially from one thread and destroys the handle.
    unsafe {
        let api = &*draft::eden_ui_v1();
        let handle = (api.create)(0);
        assert_eq!((api.event)(handle, 1, 0, 0, b"body".as_ptr(), 4), 0);

        let mut painted = Vec::<DrawnCell>::new();
        (api.render)(
            handle,
            4,
            2,
            0,
            0,
            collect_cell,
            (&mut painted as *mut Vec<DrawnCell>).cast(),
        );
        assert_eq!(
            painted[0],
            DrawnCell {
                x: 0,
                y: 0,
                fg: 0x11_2233,
                bg: COLOR_DEFAULT,
                flags: 0,
                text: "body".into(),
            }
        );
        assert_eq!(painted[1].flags, CELL_CURSOR);
        assert_eq!(painted.len(), 2);

        painted.clear();
        (api.render)(
            handle,
            4,
            2,
            1,
            1,
            collect_cell,
            (&mut painted as *mut Vec<DrawnCell>).cast(),
        );
        assert!(painted.iter().any(|cell| cell.text == "menu"));

        reset_calls();
        painted.clear();
        (api.render)(
            handle,
            4,
            2,
            3,
            0,
            collect_cell,
            (&mut painted as *mut Vec<DrawnCell>).cast(),
        );
        assert!(painted.is_empty(), "mode 3 must not reach the author");
        assert_eq!(calls(), 0);

        (api.destroy)(handle);
    }
}

#[test]
fn editor_rejects_unreadable_input_before_the_author() {
    // SAFETY: this test calls one table serially from one thread and destroys the handle.
    unsafe {
        let api = &*draft::eden_ui_v1();
        let handle = (api.create)(0);

        reset_calls();
        assert_eq!(
            (api.event)(handle, 1, 0, 0, std::ptr::null(), 4),
            EVENT_ERROR
        );
        assert_eq!(calls(), 0, "a null span must not reach the author");
        assert_eq!(
            (api.event)(handle, 1, 0, 0, b"\xff\xfe".as_ptr(), 2),
            EVENT_ERROR
        );
        assert_eq!(calls(), 0, "invalid UTF-8 must not reach the author");
        assert_eq!((api.restore)(handle, std::ptr::null(), 4, 0), -1);
        assert_eq!((api.restore)(handle, b"\xff\xfe".as_ptr(), 2, 0), -1);
        assert_eq!(calls(), 0);

        // An empty span is legal with a null pointer and reaches the author as "".
        assert_eq!((api.event)(handle, 1, 0, 0, std::ptr::null(), 0), 0);
        assert_eq!(calls(), 1);

        // A refusal keeps the previous state, so the snapshot is unchanged.
        assert_eq!(
            (api.event)(handle, 1, 0, 0, b"longer than the limit".as_ptr(), 20),
            EVENT_ERROR
        );
        let mut snapshot = Vec::<u8>::new();
        (api.snapshot)(
            handle,
            collect_bytes,
            (&mut snapshot as *mut Vec<u8>).cast(),
        );
        assert_eq!(snapshot, b"");

        (api.destroy)(handle);
    }
}

#[test]
fn editor_panics_become_the_documented_failure_values() {
    // SAFETY: this test calls one table serially from one thread and destroys the handles
    // it creates, except the one whose destructor panics on purpose.
    unsafe {
        let api = &*draft::eden_ui_v1();

        panic_at(PANIC_CREATE);
        assert!((api.create)(0).is_null());
        clear_panics();

        let handle = (api.create)(0);
        assert!(!handle.is_null());
        assert_eq!((api.event)(handle, 1, 0, 0, b"kept".as_ptr(), 4), 0);

        panic_at(PANIC_EVENT);
        assert_eq!(
            (api.event)(handle, 1, 0, 0, b"lost".as_ptr(), 4),
            EVENT_ERROR
        );
        clear_panics();

        panic_at(PANIC_RENDER);
        let mut painted = Vec::<DrawnCell>::new();
        (api.render)(
            handle,
            2,
            1,
            0,
            0,
            collect_cell,
            (&mut painted as *mut Vec<DrawnCell>).cast(),
        );
        assert!(painted.is_empty(), "a panicking frame must paint nothing");
        clear_panics();

        panic_at(PANIC_SNAPSHOT);
        let mut snapshot = Vec::<u8>::new();
        (api.snapshot)(
            handle,
            collect_bytes,
            (&mut snapshot as *mut Vec<u8>).cast(),
        );
        assert!(snapshot.is_empty(), "a panicking snapshot must not call in");
        clear_panics();

        panic_at(PANIC_CURSOR);
        assert_eq!((api.cursor)(handle), 0);
        clear_panics();

        panic_at(PANIC_RESTORE);
        assert_eq!((api.restore)(handle, b"lost".as_ptr(), 4, 0), -1);
        clear_panics();

        panic_at(PANIC_SELECTED_TEXT);
        let mut selected = Vec::<u8>::new();
        (api.selected_text)(
            handle,
            collect_bytes,
            (&mut selected as *mut Vec<u8>).cast(),
        );
        assert!(selected.is_empty());
        clear_panics();

        let other = (api.create)(0);
        panic_at(PANIC_TRANSFER);
        assert_eq!((api.transfer_state)(other, handle), -1);
        clear_panics();

        // A panic inside destroy cannot be recovered without running the destructor
        // twice; what must hold is that it does not abort the host.
        panic_at(PANIC_DESTROY);
        (api.destroy)(other);
        clear_panics();

        (api.destroy)(handle);
    }
}

#[test]
fn renderer_slots_and_failures_match_the_contract() {
    // SAFETY: this test calls one table serially from one thread and destroys the handle.
    unsafe {
        let api = &*painter::eden_renderer_v1();
        let handle = (api.create)();
        assert!(!handle.is_null());

        let mut painted = Vec::<DrawnCell>::new();
        assert_eq!(
            (api.render)(
                handle,
                b"body".as_ptr(),
                4,
                4,
                1,
                collect_cell,
                (&mut painted as *mut Vec<DrawnCell>).cast(),
            ),
            0
        );
        assert!(painted.iter().any(|cell| cell.text == "body"));
        assert!(painted.iter().any(|cell| cell.text == "light"));

        assert_eq!(
            (api.render)(
                handle,
                b"reject".as_ptr(),
                6,
                4,
                1,
                collect_cell,
                (&mut painted as *mut Vec<DrawnCell>).cast(),
            ),
            -1
        );

        panic_at(PANIC_RENDER);
        assert_eq!(
            (api.render)(
                handle,
                b"body".as_ptr(),
                4,
                4,
                1,
                collect_cell,
                (&mut painted as *mut Vec<DrawnCell>).cast(),
            ),
            -1
        );
        clear_panics();

        reset_calls();
        assert_eq!(
            (api.render)(
                handle,
                std::ptr::null(),
                4,
                4,
                1,
                collect_cell,
                (&mut painted as *mut Vec<DrawnCell>).cast(),
            ),
            -1
        );
        assert_eq!(calls(), 0, "a null payload must not reach the author");
        assert_eq!(
            (api.render)(
                handle,
                std::ptr::null(),
                0,
                4,
                1,
                collect_cell,
                (&mut painted as *mut Vec<DrawnCell>).cast(),
            ),
            0
        );
        assert_eq!(calls(), 1);

        panic_at(PANIC_CREATE);
        assert!((api.create)().is_null());
        clear_panics();

        (api.destroy)(handle);
    }
}

#[test]
fn theme_slots_and_failures_match_the_contract() {
    // SAFETY: the entry point returns a stateless table that lives as long as this
    // library; no handle is involved.
    unsafe {
        let api = &*palette::eden_theme_v1();
        assert_eq!((api.resolve)(1, theme_token::ACCENT), 0x12_ab_34);
        assert_eq!((api.resolve)(0, theme_token::ACCENT), 0xab_12_34);
        assert_eq!((api.resolve)(1, 99), COLOR_DEFAULT);

        panic_at(PANIC_RESOLVE);
        assert_eq!((api.resolve)(1, theme_token::ACCENT), COLOR_DEFAULT);
        clear_panics();
    }
}

#[test]
fn frontend_slots_and_failures_match_the_contract() {
    // SAFETY: the entry point returns a stateless table that lives as long as this
    // library; the function under test returns an exit code and owns nothing here.
    unsafe {
        let api = &*shell::eden_terminal_frontend_v1();
        assert_eq!((api.run)(b"refuse".as_ptr(), 6), 3);
        assert_eq!((api.run)(b"conf".as_ptr(), 4), 4);

        panic_at(PANIC_RUN);
        assert_eq!(
            (api.run)(b"conf".as_ptr(), 4),
            author::FRONTEND_FAILURE_EXIT
        );
        clear_panics();

        reset_calls();
        assert_eq!(
            (api.run)(std::ptr::null(), 4),
            author::FRONTEND_FAILURE_EXIT
        );
        assert_eq!(calls(), 0, "a null config must not reach the author");
        assert_eq!((api.run)(std::ptr::null(), 0), 0);
        assert_eq!(calls(), 1);
    }
}

#[test]
fn overlay_slots_and_failures_match_the_contract() {
    // SAFETY: this test calls one table serially from one thread and destroys the handle.
    unsafe {
        let api = &*modal::eden_overlay_v1();
        let handle = (api.renderer.create)();
        assert!(!handle.is_null());

        let mut painted = Vec::<DrawnCell>::new();
        assert_eq!(
            (api.renderer.render)(
                handle,
                b"{}".as_ptr(),
                2,
                4,
                1,
                collect_cell,
                (&mut painted as *mut Vec<DrawnCell>).cast(),
            ),
            0
        );
        assert!(painted.iter().any(|cell| cell.text == "modal"));

        let event = b"{\"key\":\"F(9)\"}";
        let mut reply = Vec::<u8>::new();
        assert_eq!(
            (api.event)(
                handle,
                event.as_ptr(),
                event.len(),
                collect_bytes,
                (&mut reply as *mut Vec<u8>).cast(),
            ),
            0
        );
        assert_eq!(reply, b"\"close\"");

        let mut reply = Vec::<u8>::new();
        assert_eq!(
            (api.event)(
                handle,
                b"nonsense".as_ptr(),
                8,
                collect_bytes,
                (&mut reply as *mut Vec<u8>).cast(),
            ),
            -1
        );
        assert!(reply.is_empty(), "a refused event must answer nothing");

        panic_at(PANIC_OVERLAY_EVENT);
        assert_eq!(
            (api.event)(
                handle,
                b"nonsense".as_ptr(),
                8,
                collect_bytes,
                (&mut reply as *mut Vec<u8>).cast(),
            ),
            -1
        );
        clear_panics();

        reset_calls();
        assert_eq!(
            (api.event)(
                handle,
                std::ptr::null(),
                8,
                collect_bytes,
                (&mut reply as *mut Vec<u8>).cast(),
            ),
            -1
        );
        assert_eq!(calls(), 0, "a null event must not reach the author");

        assert_eq!(
            (api.renderer.render)(
                handle,
                b"reject".as_ptr(),
                6,
                4,
                1,
                collect_cell,
                (&mut painted as *mut Vec<DrawnCell>).cast(),
            ),
            -1
        );
        panic_at(PANIC_RENDER);
        assert_eq!(
            (api.renderer.render)(
                handle,
                b"{}".as_ptr(),
                2,
                4,
                1,
                collect_cell,
                (&mut painted as *mut Vec<DrawnCell>).cast(),
            ),
            -1
        );
        clear_panics();

        panic_at(PANIC_CREATE);
        assert!((api.renderer.create)().is_null());
        clear_panics();

        (api.renderer.destroy)(handle);
    }
}
