# Native UI plugins

A terminal UI role is a trusted native library that the terminal frontend loads into its own process. It runs with the frontend's permissions and is not sandboxed. [`eden-ui-sdk`](../crates/eden-ui-sdk/src/lib.rs) owns the versioned C ABI, the safe role traits and the `export_ui!` macro that turns a trait implementation into the exported table; this page is enough to write one without reading host source.

Roles are independent: a library may export one, several or all five, and each is selected separately.

| Role | Entry point | Selection | Built-in fallback |
| --- | --- | --- | --- |
| Editor | `eden_ui_v1` | `EDEN_TUI_EDITOR`, `eden tui --editor PATH` | `ui/` library from the installation |
| Renderer | `eden_renderer_v1` | `EDEN_TUI_RENDERER` | in-process message renderer |
| Theme | `eden_theme_v1` | `EDEN_TUI_THEME` | palette from `ui.json` |
| Frontend | `eden_terminal_frontend_v1` | `EDEN_TUI_FRONTEND` | `eden tui` itself |
| Overlay | `eden_overlay_v1` | `EDEN_TUI_OVERLAY` | in-process modal presentation |

A library that fails to load, declares another ABI version, or answers null from `create` leaves the built-in role in place. Nothing is unloaded while the frontend runs, so a handle stays valid for the frontend's lifetime unless the role destroys it.

## Build an independent library

Use the SDK source from the same release, Rust 1.98.1, and a `cdylib`. The example below is the shape of [`tests/contract-authors/ui-macro`](../tests/contract-authors/ui-macro/README.md), which exports all five roles and needs nothing else from the product.

```toml
[package]
name = "my-ui-roles"
version = "0.1.0"
edition = "2024"
rust-version = "1.98.1"
license = "Apache-2.0"
[workspace]
[lib]
crate-type = ["cdylib", "rlib"]
[dependencies]
eden-ui-sdk = { path = "path/to/crates/eden-ui-sdk", version = "0.1.0" }
```

```sh
cargo build --locked --manifest-path path/to/my-ui-roles/Cargo.toml
```

The output is `libmy_ui_roles.so`, `libmy_ui_roles.dylib` or `my_ui_roles.dll`. The SDK has no dependencies, so a role that parses no JSON needs nothing beyond it; a renderer, overlay or frontend that reads JSON adds its own parser, and a frontend that talks to the host adds `eden-tui-client` and a runtime.

## Export a role

Implement the trait for the role and export the implementing type once:

```rust
eden_ui_sdk::export_ui!(theme: Palettes);
```

The arms are `editor`, `renderer`, `theme`, `frontend` and `overlay`, one role per call, and the type must implement the matching trait from `eden_ui_sdk::author`. The macro writes the entry point with the ABI name, the `#[repr(C)]` table, its version and size fields, the handle allocation, the borrowed-span decoding, the cell forwarding and the panic guard. A role implementation therefore contains no `unsafe`, no `extern "C"`, no symbol name and no table size, and it does not need to know the ABI layout. Exporting the same role twice in one library is a duplicate-symbol error at compile time.

A hand-written table remains available and supported: `UiApi`, `RendererApi`, `ThemeApi`, `TerminalFrontendApi` and `OverlayApi` are public, and the installed default editor could be replaced that way. Choose it only when the traits cannot express what the role needs, because it also gives up the handle and borrowing guarantees below.

## Role traits

Every method runs on the frontend's serialized call path: a role is never entered concurrently with itself, and it must not call back into the host from inside a method.

### Editor

```rust
pub trait Editor: Sized + 'static {
    fn create(style: u32) -> Option<Self>;
    fn event(&mut self, kind: u32, key: u32, modifiers: u32, text: &str) -> Result<(), Rejected>;
    fn render(&mut self, frame: &mut Frame<'_>);
    fn snapshot(&self) -> &str { "" }
    fn cursor(&self) -> usize { 0 }
    fn restore(&mut self, text: &str, cursor: usize) -> Result<(), Rejected> { Err(Rejected) }
    fn selected_text(&self) -> &str { "" }
    fn transfer_state(&mut self, source: &Self);
}
```

`create` receives the style the host asked for, 0 for a plain editor and 1 for a numbered one, and returns `None` to refuse it. `event` receives the ABI event verbatim: `kind` is 0 key, 1 paste, 2 pointer or 3 replace range, and `text` is that event's text, empty for events that carry none. The key numbers, the packed pointer payload and the modifier bits are documented on [`UiApi::event`](../crates/eden-ui-sdk/src/lib.rs); the entry point does not reinterpret them, so a role can ignore kinds it does not implement. `Err(Rejected)` becomes `EVENT_ERROR`, which the host does not treat as accepted input.

`render` paints one frame and receives the viewport, the mode and whether the user's terminal is dark. The entry point drops a call whose mode is not 0..=2, so a role only sees the editor, command-overlay and read-only code modes. `snapshot` returns the text the host persists, `cursor` a UTF-8 byte offset at a grapheme boundary, and `selected_text` the current selection; the defaults report an empty role rather than a failure. `restore` adopts a draft and its default refuses, so a role that cannot replace its state keeps it instead of losing it.

`transfer_state` is required and has no default: the ABI states that copying editing state between two handles preserves the destination's style, and only the implementation knows where its style lives. The entry point answers -1 for a null handle, 0 for the same handle twice without calling the method, and 0 after a completed transfer.

### Renderer

```rust
pub trait Renderer: Sized + 'static {
    fn create() -> Option<Self>;
    fn render(&mut self, payload: &[u8], frame: &mut Frame<'_>) -> Result<(), Rejected>;
}
```

`payload` is the host's UTF-8 JSON, borrowed for the call; the SDK parses nothing. Its shape is documented on [`RendererApi`](../crates/eden-ui-sdk/src/lib.rs). Returning `Err(Rejected)` makes the host discard the whole frame and keep the previous one, so cells painted before a late failure are not visible.

### Theme

```rust
pub trait Theme {
    fn resolve(dark: bool, token: u32) -> u32;
}
```

The ABI has no state slot for a theme, so resolution is an associated function with no `self`; a palette that caches anything keeps that cache in its own crate using its own synchronisation. `token` is one of the `eden_ui_sdk::theme_token` constants, and a token the palette does not know must answer `COLOR_DEFAULT` so the host keeps the user's configured colour.

### Frontend

```rust
pub trait Frontend {
    fn run(config: &[u8]) -> i32;
}
```

`config` is the host's UTF-8 JSON with `endpoint`, `session` and `plugins`, borrowed for the entire run. The return value is the process exit code.

A replacement frontend owns far more than a table: it acquires the terminal, connects to the endpoint through the public host protocol, and must release its terminal state and join every task it started before returning. The host calls `run` before taking terminal ownership, and the library stays loaded until the call returns. A panic inside `run` is caught and answers `FRONTEND_FAILURE_EXIT` (70), but a panic on a thread the frontend started is outside that guard, so a frontend must handle its own worker failures. See [interactive terminal](terminal.md) for the endpoint and session behaviour the built-in frontend implements.

### Overlay

```rust
pub trait Overlay: Sized + 'static {
    fn create() -> Option<Self>;
    fn render(&mut self, payload: &[u8], frame: &mut Frame<'_>) -> Result<(), Rejected>;
    fn event(&mut self, event: &[u8]) -> Result<&str, Rejected>;
}
```

`render` receives the modal payload documented on [`OverlayApi`](../crates/eden-ui-sdk/src/lib.rs) and follows the renderer contract. `event` receives one host event and returns the JSON reply the host acts on, for example `"close"`, `"pass"`, `"up"`, `"down"` or `"accept"`; the reply is copied through the host's callback before the method returns, so borrowing it from `self` is enough. `Err(Rejected)` answers -1 without a reply and leaves ordinary host key handling in charge. One state value serves `render` and `event`, which is what the overlay table's single state pointer means.

## The render frame

`Frame` is the only way a role paints, and it exists for exactly one call:

```rust
impl eden_ui_sdk::author::Renderer for Banner {
    fn create() -> Option<Self> { Some(Self) }

    fn render(&mut self, payload: &[u8], frame: &mut Frame<'_>) -> Result<(), Rejected> {
        if frame.width() == 0 || frame.height() == 0 {
            return Ok(());
        }
        frame.set(0, 0, "banner", CellStyle::default());
        frame.cursor(0, 0);
        let _ = payload;
        Ok(())
    }
}
```

`width`, `height`, `mode` and `dark` describe the call. `set` paints one string at a viewport-relative position with a `CellStyle` of foreground, background and `CELL_*` flags; `cursor` places the terminal cursor for IME without painting. A position outside the viewport is forwarded and dropped by the host, so a role that needs to know what fit reads `width` and `height` first. `CellStyle::default()` is terminal-default foreground and background with no flags, which is what a role that sets no colour means.

The frame holds the host's cell callback, so it must not outlive the call: it cannot be stored in a `'static` owner, and the borrowed text of one `set` call is read by the host before the sink returns.

## Ownership, borrowing and failure

Input spans are borrowed for the call that received them. A role that keeps bytes must copy them; a role must not retain the pointer, spawn work that reads it after the method returns, or hand it to another thread. Handles are owned by the library that created them and are released exactly once through `destroy`, which the host calls before the frontend exits.

Every slot catches a panic and answers the value its contract defines instead of unwinding across C. The role's own state can be left inconsistent by that panic, which is the author's responsibility; the guard only guarantees that the frontend survives.

| Slot | Panic or refusal answer |
| --- | --- |
| `create` (editor, renderer, overlay) | null handle; the host keeps the library and its built-in role |
| `destroy` | nothing; a panic there can leak the handle it was destroying |
| editor `event` | `EVENT_ERROR` |
| editor `render` | nothing further is painted; cells already sent stay |
| editor `snapshot`, `selected_text` | the byte callback is not called, so the host reads an empty result |
| editor `cursor` | 0 |
| editor `restore` | -1; the previous draft and cursor are kept |
| editor `transfer_state` | -1; the destination is kept |
| renderer `render` | -1; the host discards the frame and keeps the previous one |
| theme `resolve` | `COLOR_DEFAULT` |
| frontend `run` | `FRONTEND_FAILURE_EXIT`, 70 |
| overlay `render` | -1; the host keeps its built-in modal |
| overlay `event` | -1; ordinary key handling continues |

The entry points also reject input the ABI forbids before the role sees it: a null pointer with a nonzero length and text that is not UTF-8 answer that slot's failure value instead of reaching the implementation.

## Minimal complete roles

A palette, a renderer and an editor cover the three shapes a role takes: stateless, stateful without input, and stateful with input.

```rust
use eden_ui_sdk::{COLOR_DEFAULT, author, theme_token};

struct Palettes;

impl author::Theme for Palettes {
    fn resolve(dark: bool, token: u32) -> u32 {
        match (dark, token) {
            (true, theme_token::ACCENT) => eden_ui_sdk::mocha::MAUVE,
            _ => COLOR_DEFAULT,
        }
    }
}

eden_ui_sdk::export_ui!(theme: Palettes);
```

```rust
use eden_ui_sdk::{
    COLOR_DEFAULT, author,
    author::{CellStyle, Frame, Rejected},
};

struct Messages;

impl author::Renderer for Messages {
    fn create() -> Option<Self> {
        Some(Self)
    }

    fn render(&mut self, payload: &[u8], frame: &mut Frame<'_>) -> Result<(), Rejected> {
        let payload: serde_json::Value = serde_json::from_slice(payload).map_err(|_| Rejected)?;
        let count = payload["messages"]
            .as_array()
            .ok_or(Rejected)?
            .len();
        frame.set(
            0,
            0,
            &format!("{count} messages"),
            CellStyle {
                fg: COLOR_DEFAULT,
                bg: COLOR_DEFAULT,
                flags: eden_ui_sdk::CELL_BOLD,
            },
        );
        Ok(())
    }
}

eden_ui_sdk::export_ui!(renderer: Messages);
```

```rust
use eden_ui_sdk::{
    author,
    author::{CellStyle, Frame, Rejected},
};

#[derive(Default)]
struct Draft {
    text: String,
    cursor: usize,
}

impl author::Editor for Draft {
    fn create(style: u32) -> Option<Self> {
        (style <= 1).then(Self::default)
    }

    fn event(&mut self, kind: u32, key: u32, _modifiers: u32, text: &str) -> Result<(), Rejected> {
        match kind {
            1 | 3 => {
                self.text.push_str(text);
                self.cursor = self.text.len();
            }
            0 if key == 7 => {
                self.text.pop();
                self.cursor = self.text.len();
            }
            _ => {}
        }
        Ok(())
    }

    fn render(&mut self, frame: &mut Frame<'_>) {
        if frame.width() == 0 || frame.height() == 0 {
            return;
        }
        frame.set(0, 0, &self.text, CellStyle::default());
        frame.cursor((self.cursor as u16).min(frame.width() - 1), 0);
    }

    fn snapshot(&self) -> &str {
        &self.text
    }

    fn cursor(&self) -> usize {
        self.cursor
    }

    fn restore(&mut self, text: &str, cursor: usize) -> Result<(), Rejected> {
        if cursor > text.len() {
            return Err(Rejected);
        }
        self.text = text.to_owned();
        self.cursor = cursor;
        Ok(())
    }

    fn transfer_state(&mut self, source: &Self) {
        self.text.clone_from(&source.text);
        self.cursor = source.cursor;
    }
}

eden_ui_sdk::export_ui!(editor: Draft);
```

## Verify a role

Unit-test the trait implementation directly, then test the exported table once: `eden_ui_v1()` and the other entry points return the table, and driving `create`, `event`, `snapshot` and `destroy` through it is the same path the host takes. `cargo build --locked` plus `cargo clippy --all-targets --locked -- -D warnings` with `unsafe_op_in_unsafe_fn` and `missing_docs` denied proves the role carries no `unsafe` of its own.

For installed acceptance, run the product's `python3 scripts/verify-tui.py`: it loads an installed frontend with a real PTY and drives both [`tests/contract-authors/tui-editor`](../tests/contract-authors/tui-editor/README.md) and the macro-authored [`tests/contract-authors/ui-macro`](../tests/contract-authors/ui-macro/README.md) through the same assertions.
