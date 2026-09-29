//! Native UI C ABI. No Rust layout, allocator, or framework types cross this boundary.
//!
//! Calls are serialized by the host. Handles belong to the library that created them;
//! destroy all handles before unloading it. Input pointers are borrowed for a call.
//! Callback pointers (including text) are borrowed only until the callback returns.
//! Callbacks must not unwind, retain pointers, or reenter the plugin.
//!
//! A role may instead implement the safe traits in [`author`] and export them with
//! [`export_ui!`], which keeps the tables above and the entry points out of the author's
//! file without changing this ABI.
use std::ffi::c_void;

pub mod author;
mod export;

/// Reject tables with any other version before reading function pointers.
pub const ABI_VERSION: u32 = 1;
/// Use the terminal's configured default colour rather than an RGB colour.
pub const COLOR_DEFAULT: u32 = u32::MAX;
/// Request bold text without changing the terminal palette.
pub const CELL_BOLD: u32 = 1;
/// Request an underline for links or focus.
pub const CELL_UNDERLINE: u32 = 2;
/// Express selection without assuming the terminal background.
pub const CELL_REVERSE: u32 = 4;
/// Position the terminal cursor for IME; the text is empty.
pub const CELL_CURSOR: u32 = 0x8000_0000;
/// An invalid event must not be treated as successful input.
pub const EVENT_ERROR: u32 = u32::MAX;

/// All coordinates are relative to the viewport supplied to `render`.
/// Colors are packed 0xRRGGBB, or `COLOR_DEFAULT`. Cursor cells contain empty text and only set position.
#[repr(C)]
pub struct Cell {
    /// Zero-based column relative to the viewport.
    pub x: u16,
    /// Zero-based row relative to the viewport.
    pub y: u16,
    /// Foreground RGB or COLOR_DEFAULT.
    pub fg: u32,
    /// Background RGB or COLOR_DEFAULT.
    pub bg: u32,
    /// Combination of CELL flags.
    pub flags: u32,
    /// Borrowed UTF-8 for this callback only.
    pub text: *const u8,
    /// Byte length; zero permits a null text pointer.
    pub text_len: usize,
}

/// Consumes one borrowed cell synchronously; must not unwind or reenter.
pub type CellSink = extern "C" fn(*mut c_void, *const Cell);
/// Consumes borrowed bytes synchronously; copy before returning.
pub type ByteSink = extern "C" fn(*mut c_void, *const u8, usize);

/// Read this prefix first, before dereferencing a full `UiApi`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ApiHeader {
    /// Must equal ABI_VERSION before the table is dereferenced.
    pub abi: u32,
    /// Must cover the complete requested table before any function is read.
    pub table_size: u32,
}

/// Native trusted-plugin boundary, not a sandbox. The host validates the header
/// before reading this table. Invalid/dangling arbitrary pointers cannot be checked.
#[repr(C)]
pub struct UiApi {
    /// Must equal ABI_VERSION before the table is dereferenced.
    pub abi: u32,
    /// Must cover the complete requested table before any function is read.
    pub table_size: u32,
    /// Style: 0 plain editor, 1 numbered editor.
    pub create: unsafe extern "C" fn(u32) -> *mut c_void,
    /// Release a live handle exactly once through its creating library.
    pub destroy: unsafe extern "C" fn(*mut c_void),
    /// Kind: 0 key, 1 paste, 2 pointer, 3 replace range.
    /// Replace range: key=start byte, modifiers=end byte, text=replacement.
    /// Both endpoints must be grapheme boundaries; one undo step, cursor follows insertion.
    /// Pointer: key packs viewport-relative (row << 16) | column; modifiers bit 0 extends
    /// selection for dragging or Shift-click. Requires a preceding editor render.
    /// Keys: left/right/up/down/home/end/backspace/delete/newline/char = 1..10.
    /// Extra keys: selectAll=11, undo=12, redo=13, deleteWordBack=14, killToEnd=15,
    /// yank=16, wordLeft=17, wordRight=18, killToStart=19, yankPop=20.
    /// Modifiers: shift=1, ctrl=2, alt=4. Returns 0 or EVENT_ERROR; never submits.
    pub event: unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const u8, usize) -> u32,
    /// Mode: 0 editor, 1 command overlay, 2 read-only code renderer. Dark: 0 light, 1 dark.
    pub render: unsafe extern "C" fn(*mut c_void, u16, u16, u32, u32, CellSink, *mut c_void),
    /// Copy current UTF-8 through the borrowed byte callback.
    pub snapshot: unsafe extern "C" fn(*mut c_void, ByteSink, *mut c_void),
    /// Cursor is a UTF-8 byte offset at a grapheme boundary.
    pub cursor: unsafe extern "C" fn(*mut c_void) -> usize,
    /// Returns 0, or -1 for invalid UTF-8/cursor/input. Failure preserves state.
    pub restore: unsafe extern "C" fn(*mut c_void, *const u8, usize, usize) -> i32,
    /// Returns the selected UTF-8 bytes; empty when selection is collapsed.
    pub selected_text: unsafe extern "C" fn(*mut c_void, ByteSink, *mut c_void),
    /// Copies all editing state between handles created by this library, preserving destination style.
    pub transfer_state: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32,
}

/// Shared Catppuccin Mocha foregrounds. Source and MIT notice: themes/README.md.
pub mod mocha {
    /// Catppuccin Mocha foreground RGB; never applied as the default background.
    pub const TEXT: u32 = 0xcdd6f4;
    /// Catppuccin Mocha foreground RGB; never applied as the default background.
    pub const SUBTEXT: u32 = 0xa6adc8;
    /// Catppuccin Mocha foreground RGB; never applied as the default background.
    pub const OVERLAY: u32 = 0x7f849c;
    /// Catppuccin Mocha foreground RGB; never applied as the default background.
    pub const BORDER: u32 = 0x6c7086;
    /// Catppuccin Mocha foreground RGB; never applied as the default background.
    pub const MAUVE: u32 = 0xcba6f7;
    /// Catppuccin Mocha foreground RGB; never applied as the default background.
    pub const GREEN: u32 = 0xa6e3a1;
    /// Catppuccin Mocha foreground RGB; never applied as the default background.
    pub const RED: u32 = 0xf38ba8;
    /// Catppuccin Mocha foreground RGB; never applied as the default background.
    pub const BLUE: u32 = 0x89b4fa;
    /// Catppuccin Mocha foreground RGB; never applied as the default background.
    pub const YELLOW: u32 = 0xf9e2af;
    /// Catppuccin Mocha foreground RGB; never applied as the default background.
    pub const PEACH: u32 = 0xfab387;
    /// Catppuccin Mocha foreground RGB; never applied as the default background.
    pub const TEAL: u32 = 0x94e2d5;
    /// Catppuccin Mocha foreground RGB; never applied as the default background.
    pub const LAVENDER: u32 = 0xb4befe;
    /// Catppuccin Mocha foreground RGB; never applied as the default background.
    pub const PINK: u32 = 0xf5c2e7;
}

/// Replaceable structured message renderer, exported as `eden_renderer_v1`.
/// Input is UTF-8 JSON: `{ "messages": [Message], "theme": { "foreground": RGB }, "scroll": 0 }`.
/// Each message carries id, role, title, body, expanded, failed, pending, before/after and revision. Optional preview text is an explicit collapsed summary; the complete body remains available. Theme values are RGB integers or COLOR_DEFAULT; preferences and session identify the local view. Cells are viewport-relative.
/// Calls and callbacks follow the module ownership and serialization contract.
#[repr(C)]
pub struct RendererApi {
    /// Validate before reading the rest of the table.
    pub header: ApiHeader,
    /// Null reports construction failure; the host keeps the library loaded.
    pub create: unsafe extern "C" fn() -> *mut c_void,
    /// Consume the creating library's handle after the last render returns.
    pub destroy: unsafe extern "C" fn(*mut c_void),
    /// Borrow JSON and emit cells synchronously; return zero or -1 on invalid input.
    pub render:
        unsafe extern "C" fn(*mut c_void, *const u8, usize, u16, u16, CellSink, *mut c_void) -> i32,
}

/// Replaceable semantic palette, exported as `eden_theme_v1`.
#[repr(C)]
pub struct ThemeApi {
    /// Validate before reading the resolver.
    pub header: ApiHeader,
    /// Dark is 0 for light or 1 for dark; unknown tokens return COLOR_DEFAULT.
    /// Resolve on the calling thread without retaining any host state.
    pub resolve: unsafe extern "C" fn(u32, u32) -> u32,
}

/// Replaceable terminal application, exported as `eden_terminal_frontend_v1`.
/// The host calls `run` before taking terminal ownership. The plugin owns terminal
/// setup and restoration, connects to the endpoint using the public host protocol,
/// and returns only after its tasks and terminal resources have been released.
#[repr(C)]
pub struct TerminalFrontendApi {
    /// Validate before entering plugin code.
    pub header: ApiHeader,
    /// Borrow UTF-8 JSON config for the entire run; return the process exit code.
    /// Config contains `endpoint` (string), `session` (string or null), and
    /// `plugins` (object mapping editor/renderer/theme to paths or null).
    /// Callbacks and Rust objects must not cross this boundary; the frontend
    /// communicates with the host over its endpoint. Never unwind across C.
    pub run: unsafe extern "C" fn(*const u8, usize) -> i32,
}

/// Semantic IDs shared by built-in and external palettes; RGB or COLOR_DEFAULT.
pub mod theme_token {
    /// Palette role for text surfaces or foregrounds.
    pub const TEXT: u32 = 0;
    /// Palette role for muted surfaces or foregrounds.
    pub const MUTED: u32 = 1;
    /// Palette role for accent surfaces or foregrounds.
    pub const ACCENT: u32 = 2;
    /// Palette role for background surfaces or foregrounds.
    pub const BACKGROUND: u32 = 3;
    /// Palette role for success surfaces or foregrounds.
    pub const SUCCESS: u32 = 4;
    /// Palette role for error surfaces or foregrounds.
    pub const ERROR: u32 = 5;
    /// Palette role for border surfaces or foregrounds.
    pub const BORDER: u32 = 6;
    /// Palette role for panel surfaces or foregrounds.
    pub const PANEL: u32 = 7;
    /// Palette role for selected surfaces or foregrounds.
    pub const SELECTED: u32 = 8;
    /// Palette role for heading surfaces or foregrounds.
    pub const HEADING: u32 = 9;
    /// Palette role for key surfaces or foregrounds.
    pub const KEY: u32 = 10;
    /// Palette role for code surfaces or foregrounds.
    pub const CODE: u32 = 11;
    /// Palette role for link surfaces or foregrounds.
    pub const LINK: u32 = 12;
    /// Palette role for quote surfaces or foregrounds.
    pub const QUOTE: u32 = 13;
    /// Palette role for number surfaces or foregrounds.
    pub const NUMBER: u32 = 14;
    /// Palette role for type surfaces or foregrounds.
    pub const TYPE: u32 = 15;
    /// Palette role for function surfaces or foregrounds.
    pub const FUNCTION: u32 = 16;
    /// Palette role for dim surfaces or foregrounds.
    pub const DIM: u32 = 17;
    /// Palette role for warning surfaces or foregrounds.
    pub const WARNING: u32 = 18;
}

/// Replaceable modal presentation and key mapping, exported as `eden_overlay_v1`.
/// The renderer receives `{kind, title, query, selected, items, fields, theme}`;
/// field values are masked when private. Host controls retain action validation and mouse targets.
/// `event` receives `{key, modifiers}` (Crossterm key names; shift=1, ctrl=2, alt=4)
/// and emits one JSON string: `pass`, `up`, `down`, `accept`, or `close`.
/// Invalid replies fall back to the original event. No framework values cross this table.
#[repr(C)]
pub struct OverlayApi {
    /// The common rendering prefix follows RendererApi ownership and callback rules.
    pub renderer: RendererApi,
    /// Borrow the event and copy the response through the callback before returning zero.
    /// A negative result leaves ordinary host key handling in charge.
    pub event: unsafe extern "C" fn(*mut c_void, *const u8, usize, ByteSink, *mut c_void) -> i32,
}
