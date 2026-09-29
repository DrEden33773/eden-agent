//! Safe role traits, the render frame and the borrowing helpers behind [`crate::export_ui!`].
//!
//! A role author implements one of [`Editor`], [`Renderer`], [`Theme`], [`Frontend`] or
//! [`Overlay`] and exports it with [`crate::export_ui!`]. Everything the C ABI needs and
//! a safe implementation cannot express stays in that macro and in this module: the
//! handle is a [`Box`] of the author's own type, the input span is decoded once, the cell
//! callback is reachable only through [`Frame`], and every slot catches a panic and
//! answers the failure value its contract defines.
//!
//! The ownership rules of the crate header still apply here. The host serializes calls,
//! so an implementation never runs concurrently with itself; the callback pointer and any
//! text borrowed through it are valid only until the sink returns, so an implementation
//! must copy what it keeps; and a slot must not reenter the host.
use std::{ffi::c_void, marker::PhantomData};

use crate::{CELL_CURSOR, COLOR_DEFAULT, Cell, CellSink};

/// Exit code for a frontend that panicked or received an unreadable configuration span.
///
/// The ABI returns this value as the process exit code, so it must not be zero: a
/// frontend that never ran must not look like one that finished. The value follows the
/// `EX_SOFTWARE` convention for an internal failure.
pub const FRONTEND_FAILURE_EXIT: i32 = 70;

/// A refused call; the host sees the failure value of that slot.
///
/// There is no payload because the ABI has nowhere to carry one: `event`, `restore` and
/// the renderer slots report only success or failure, and each failure already keeps the
/// host's previous state. A role that needs to tell the user why reports it through its
/// own cells or snapshot instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rejected;

/// Foreground, background and flags for one [`Frame::set`] call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellStyle {
    /// Foreground RGB or [`COLOR_DEFAULT`].
    pub fg: u32,
    /// Background RGB or [`COLOR_DEFAULT`].
    pub bg: u32,
    /// Combination of the crate's `CELL_*` flags; zero paints plain text.
    pub flags: u32,
}

impl Default for CellStyle {
    /// Terminal default foreground and background with no flags.
    ///
    /// The default is not `0`: a zero colour is opaque black, while [`COLOR_DEFAULT`]
    /// keeps the user's configured palette, which is what an author who sets no colour
    /// means.
    fn default() -> Self {
        Self {
            fg: COLOR_DEFAULT,
            bg: COLOR_DEFAULT,
            flags: 0,
        }
    }
}

/// One render call's viewport, mode and cell sink.
///
/// The frame is valid only for the call it is passed to. That window is what lets the
/// sink forward a borrowed `&str` instead of copying every cell: the host reads the text
/// before the sink returns. The lifetime parameter keeps a frame from being stored in a
/// `'static` owner, where the pointers it holds would outlive the call.
pub struct Frame<'a> {
    sink: CellSink,
    ctx: *mut c_void,
    width: u16,
    height: u16,
    mode: u32,
    dark: bool,
    _call: PhantomData<&'a mut ()>,
}

impl Frame<'_> {
    /// Viewport width in cells; zero means the host has no room to draw.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Viewport height in cells; zero means the host has no room to draw.
    pub fn height(&self) -> u16 {
        self.height
    }

    /// Render mode of an editor frame: 0 editor, 1 command overlay, 2 read-only code.
    ///
    /// Tables whose ABI has no mode slot always report 0. The value is the raw ABI
    /// integer rather than an enum, so a role never fails to compile against a mode a
    /// later ABI adds.
    pub fn mode(&self) -> u32 {
        self.mode
    }

    /// Whether the host is using a dark palette. Tables without the flag report `false`
    /// and pass the palette they need in their payload instead.
    pub fn dark(&self) -> bool {
        self.dark
    }

    /// Paint `text` at viewport-relative `x`, `y`.
    ///
    /// The cell is forwarded unchanged: a call outside the viewport is not clipped or
    /// counted here, and the host drops it, so a role that must know what fit reads
    /// [`Frame::width`] and [`Frame::height`] first.
    pub fn set(&mut self, x: u16, y: u16, text: &str, style: CellStyle) {
        (self.sink)(
            self.ctx,
            &Cell {
                x,
                y,
                fg: style.fg,
                bg: style.bg,
                flags: style.flags,
                text: text.as_ptr(),
                text_len: text.len(),
            },
        );
    }

    /// Place the terminal cursor for IME at `x`, `y` without painting text.
    ///
    /// The cursor cell carries no text and a flag the host checks before the text
    /// pointer, so it cannot be mistaken for a painted glyph.
    pub fn cursor(&mut self, x: u16, y: u16) {
        (self.sink)(
            self.ctx,
            &Cell {
                x,
                y,
                fg: COLOR_DEFAULT,
                bg: COLOR_DEFAULT,
                flags: CELL_CURSOR,
                text: std::ptr::null(),
                text_len: 0,
            },
        );
    }
}

impl<'a> Frame<'a> {
    /// Build the frame for one generated role call.
    ///
    /// This exists for the expansion of [`crate::export_ui!`], which is the only caller
    /// that can satisfy the contract below; a role receives its frame from the trait
    /// method instead of building one.
    ///
    /// # Safety
    /// `sink` must be the host's cell callback and `ctx` the context it expects, both
    /// alive for the entire call, and the returned frame must not outlive them.
    #[doc(hidden)]
    pub unsafe fn new(
        sink: CellSink,
        ctx: *mut c_void,
        width: u16,
        height: u16,
        mode: u32,
        dark: bool,
    ) -> Self {
        Self {
            sink,
            ctx,
            width,
            height,
            mode,
            dark,
            _call: PhantomData,
        }
    }
}

/// Borrow `len` bytes as UTF-8 for the duration of the current call.
///
/// A null pointer with a nonzero length and bytes that are not UTF-8 both return `None`,
/// which the generated slots report as their failure value rather than passing text the
/// ABI forbids to the author.
///
/// # Safety
/// `ptr` must be null or readable for `len` initialized bytes that stay alive and
/// unmodified for the returned borrow, and the caller must not keep the borrow past the
/// call that lent it.
pub unsafe fn borrow_str<'a>(ptr: *const u8, len: usize) -> Option<&'a str> {
    // SAFETY: this call passes the same contract it received to the byte helper.
    let bytes = unsafe { borrow_bytes(ptr, len) }?;
    std::str::from_utf8(bytes).ok()
}

/// Borrow `len` bytes for the duration of the current call.
///
/// This is the byte-level form for slots whose payload is JSON: the crate parses nothing,
/// so a role brings its own parser. A zero length yields an empty slice even for a null
/// pointer, which the ABI allows.
///
/// # Safety
/// `ptr` must be null or readable for `len` initialized bytes that stay alive and
/// unmodified for the returned borrow, and the caller must not keep the borrow past the
/// call that lent it.
pub unsafe fn borrow_bytes<'a>(ptr: *const u8, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        return Some(&[]);
    }
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees readability for len bytes for the returned borrow.
    Some(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// A native editor behind `eden_ui_v1`.
pub trait Editor: Sized + 'static {
    /// Build the editor for an ABI style: 0 plain, 1 numbered.
    ///
    /// `None` refuses the style, which keeps the host from enabling a role it cannot
    /// construct; the library stays loaded either way.
    fn create(style: u32) -> Option<Self>;

    /// Apply one host event and report whether it was accepted.
    ///
    /// `kind` is 0 key, 1 paste, 2 pointer or 3 replace range, with the keys, the packed
    /// pointer payload and the modifier bits documented on [`crate::UiApi::event`].
    /// `text` is that event's text, decoded once by the entry point and empty for the
    /// events that carry none. `Err(Rejected)` becomes `EVENT_ERROR`, which the host
    /// must not treat as accepted input.
    fn event(&mut self, kind: u32, key: u32, modifiers: u32, text: &str) -> Result<(), Rejected>;

    /// Paint the current state into the viewport.
    ///
    /// The entry point drops a call whose mode is not 0..=2, so this runs only for the
    /// editor, command-overlay and read-only code modes of [`crate::UiApi::render`].
    fn render(&mut self, frame: &mut Frame<'_>);

    /// The complete text the host persists.
    ///
    /// The default is empty: a role that stores no text must still answer a snapshot
    /// request, and an empty snapshot is the truthful answer rather than a failure.
    fn snapshot(&self) -> &str {
        ""
    }

    /// The cursor as a UTF-8 byte offset at a grapheme boundary.
    ///
    /// The default reports the start of the text, which is where a role without a cursor
    /// would put it.
    fn cursor(&self) -> usize {
        0
    }

    /// Replace the draft with `text` and place the cursor at byte `cursor`.
    ///
    /// `Err(Rejected)` becomes -1 and leaves the previous state, cursor included,
    /// untouched. The default refuses every restore, so a role that cannot adopt a draft
    /// keeps its own state instead of silently discarding it.
    fn restore(&mut self, text: &str, cursor: usize) -> Result<(), Rejected> {
        let _ = (text, cursor);
        Err(Rejected)
    }

    /// The selected text, empty when the selection is collapsed.
    ///
    /// The default reports no selection, which the host renders as an empty selection
    /// rather than a stale one.
    fn selected_text(&self) -> &str {
        ""
    }

    /// Copy all editing state from another handle while keeping this handle's style.
    ///
    /// Required rather than defaulted: the ABI states that the destination style
    /// survives the transfer, and only the implementation knows where its style lives.
    /// The entry point answers -1 for a null handle and 0 for two equal handles without
    /// calling this method.
    fn transfer_state(&mut self, source: &Self);
}

/// A structured message renderer behind `eden_renderer_v1`.
pub trait Renderer: Sized + 'static {
    /// Build the renderer.
    ///
    /// `None` refuses to construct one; the host then keeps its built-in renderer, which
    /// is also what happens when this call panics.
    fn create() -> Option<Self>;

    /// Render one payload, painting cells through `frame`.
    ///
    /// `payload` is the host's UTF-8 JSON, borrowed for this call only; this crate parses
    /// nothing, so a role brings its own parser. `Err(Rejected)` becomes -1, and the host
    /// discards the whole frame and keeps the previous one, so cells painted before a
    /// late failure are not visible.
    fn render(&mut self, payload: &[u8], frame: &mut Frame<'_>) -> Result<(), Rejected>;
}

/// A semantic palette behind `eden_theme_v1`.
///
/// The ABI has no state slot, so resolution is an associated function: a palette that
/// caches anything keeps that cache in its own crate, and the host may call it from any
/// thread that serializes the table's calls.
pub trait Theme {
    /// Resolve a dark-mode flag and one of the [`crate::theme_token`] IDs to RGB.
    ///
    /// A token the palette does not know must answer [`COLOR_DEFAULT`] so the host uses
    /// the user's configured colour instead of an invented one; a panic answers the same
    /// value.
    fn resolve(dark: bool, token: u32) -> u32;
}

/// A terminal application behind `eden_terminal_frontend_v1`.
pub trait Frontend {
    /// Run the application with the host's UTF-8 JSON configuration and report the
    /// process exit code.
    ///
    /// The configuration is borrowed for the entire call. The role owns the terminal,
    /// the endpoint connection and every task it starts, and must release them before
    /// returning; see [`crate::TerminalFrontendApi::run`] for that lifecycle. A panic in
    /// this function is caught and answers [`FRONTEND_FAILURE_EXIT`], but a panic on a
    /// thread the role started is outside that catch.
    fn run(config: &[u8]) -> i32;
}

/// A modal presenter behind `eden_overlay_v1`.
pub trait Overlay: Sized + 'static {
    /// Build the modal.
    ///
    /// `None` refuses to construct one; the host then keeps its built-in modal.
    fn create() -> Option<Self>;

    /// Render one payload, painting cells through `frame`.
    ///
    /// The payload and failure contract is the one [`Renderer::render`] states: the
    /// frame is discarded as a whole when this returns `Err(Rejected)`.
    fn render(&mut self, payload: &[u8], frame: &mut Frame<'_>) -> Result<(), Rejected>;

    /// Map one host event to the JSON reply the host acts on.
    ///
    /// The event is the host's UTF-8 JSON, and the reply is copied through the host's
    /// callback before this returns, so a reply borrowed from `self` is enough.
    /// `Err(Rejected)` answers -1 without calling the callback, which leaves ordinary
    /// host key handling in charge.
    fn event(&mut self, event: &[u8]) -> Result<&str, Rejected>;
}
