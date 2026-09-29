# Macro-authored native UI roles

This separately built dynamic library declares the same five native UI roles as
[`tui-editor`](../tui-editor/README.md), written with the SDK's safe role traits and
`export_ui!` instead of a hand-written function table. It contains no `unsafe`, no
`extern "C"` function, no `#[repr(C)]` type and no entry point: the macro writes all of
them, and the crate still denies `unsafe_op_in_unsafe_fn`, `missing_docs` and
`clippy::undocumented_unsafe_blocks` the way the product workspace does.

Its observable output is deliberately the one of the hand-written fixture: an `A` editor
marker with an append-only draft, an `AUTHOR RENDERER` frame, an `AUTHOR OVERLAY` modal
whose F9 closes it, the `12ab34` accent for the semantic accent token and an
`AUTHOR FRONTEND session` line. `verify-tui.py` runs both libraries through the same
installed assertions, so the pair is the A/B evidence that a macro-exported table behaves
like a table an author wrote by hand.

Build after installing the host: `cargo build --locked --manifest-path tests/contract-authors/ui-macro/Cargo.toml`.
The output is `libauthor_ui_macro.so`, `libauthor_ui_macro.dylib`, or `author_ui_macro.dll`.
Both libraries are selected through the same settings: `EDEN_TUI_EDITOR`,
`EDEN_TUI_RENDERER`, `EDEN_TUI_THEME`, `EDEN_TUI_OVERLAY` and `EDEN_TUI_FRONTEND`.
