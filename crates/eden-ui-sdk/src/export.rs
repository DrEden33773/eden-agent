//! The [`crate::export_ui!`] macro: one call per role, no `unsafe` in the author's file.
//!
//! The `SAFETY` comments for the `unsafe` blocks this macro expands live in the macro
//! body, next to the operation they justify, because that is where the contract is
//! decided. `clippy::undocumented_unsafe_blocks` reads them from the definition whenever
//! the expansion and the definition share a crate.

/// Export a native UI role from a `cdylib`.
///
/// One call exports one role: `editor:` defines `eden_ui_v1`, `renderer:` defines
/// `eden_renderer_v1`, `theme:` defines `eden_theme_v1`, `frontend:` defines
/// `eden_terminal_frontend_v1` and `overlay:` defines `eden_overlay_v1`. The type after
/// the colon implements the matching trait from [`crate::author`], and the macro writes
/// the `#[repr(C)]` table, the entry point and the C functions, so the role's own code
/// carries no `unsafe`, no symbol name and no table size.
///
/// The generated table takes its version and its size from this crate's types rather than
/// from the author, and the overlay arm sizes its nested renderer header for the complete
/// `OverlayApi`; a hand-written table that reuses the renderer size is the mistake that
/// removes. All five roles may be exported from one library, and exporting the same role
/// twice is a duplicate-symbol error at compile time rather than a silent link-time
/// override, which is why the arms are separate calls.
///
/// The hand-written table path stays public: a role that needs control the traits do not
/// offer builds `UiApi` and the entry point itself.
///
/// # Example
///
/// A minimal palette; the editor, renderer, frontend and overlay arms take the same shape.
///
/// ```ignore
/// struct Palettes;
///
/// impl eden_ui_sdk::author::Theme for Palettes {
///     fn resolve(dark: bool, token: u32) -> u32 {
///         if dark && token == eden_ui_sdk::theme_token::TEXT {
///             eden_ui_sdk::mocha::TEXT
///         } else {
///             eden_ui_sdk::COLOR_DEFAULT
///         }
///     }
/// }
///
/// eden_ui_sdk::export_ui!(theme: Palettes);
/// ```
#[macro_export]
macro_rules! export_ui {
    (editor: $ty:ty $(,)?) => {
        // The exported entry point is named by the ABI, not by an author writing
        // documentation; the macro's own doc comment covers it.
        #[allow(missing_docs)]
        #[unsafe(no_mangle)]
        pub extern "C" fn eden_ui_v1() -> *const $crate::UiApi {
            unsafe extern "C" fn create(style: u32) -> *mut ::core::ffi::c_void {
                ::std::panic::catch_unwind(|| <$ty as $crate::author::Editor>::create(style))
                    .ok()
                    .flatten()
                    .map_or(::core::ptr::null_mut(), |editor| {
                        ::std::boxed::Box::into_raw(::std::boxed::Box::new(editor))
                            .cast::<::core::ffi::c_void>()
                    })
            }
            unsafe extern "C" fn destroy(state: *mut ::core::ffi::c_void) {
                let _ = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    if state.is_null() {
                        return;
                    }
                    // SAFETY: the host returns a handle this library allocated and
                    // releases it once; the null case returned above.
                    drop(unsafe { ::std::boxed::Box::from_raw(state.cast::<$ty>()) });
                }));
            }
            unsafe extern "C" fn event(
                state: *mut ::core::ffi::c_void,
                kind: u32,
                key: u32,
                modifiers: u32,
                text: *const u8,
                len: usize,
            ) -> u32 {
                ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    // SAFETY: the host serializes calls and keeps its handle alive for
                    // this one.
                    let Some(editor) = (unsafe { state.cast::<$ty>().as_mut() }) else {
                        return $crate::EVENT_ERROR;
                    };
                    // SAFETY: the host lends the event text for this call, and the borrow
                    // ends with it.
                    let Some(text) = (unsafe { $crate::author::borrow_str(text, len) }) else {
                        return $crate::EVENT_ERROR;
                    };
                    match <$ty as $crate::author::Editor>::event(editor, kind, key, modifiers, text)
                    {
                        Ok(()) => 0,
                        Err(_) => $crate::EVENT_ERROR,
                    }
                }))
                .unwrap_or($crate::EVENT_ERROR)
            }
            unsafe extern "C" fn render(
                state: *mut ::core::ffi::c_void,
                width: u16,
                height: u16,
                mode: u32,
                dark: u32,
                sink: $crate::CellSink,
                ctx: *mut ::core::ffi::c_void,
            ) {
                let _ = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    if mode > 2 {
                        return;
                    }
                    // SAFETY: the host serializes calls and keeps its handle alive for
                    // this one.
                    let Some(editor) = (unsafe { state.cast::<$ty>().as_mut() }) else {
                        return;
                    };
                    // SAFETY: the host lends its cell callback and context for this call,
                    // and the frame does not outlive them.
                    let mut frame = unsafe {
                        $crate::author::Frame::new(sink, ctx, width, height, mode, dark != 0)
                    };
                    <$ty as $crate::author::Editor>::render(editor, &mut frame);
                }));
            }
            unsafe extern "C" fn snapshot(
                state: *mut ::core::ffi::c_void,
                sink: $crate::ByteSink,
                ctx: *mut ::core::ffi::c_void,
            ) {
                let _ = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    // SAFETY: the host serializes calls and keeps its handle alive for
                    // this one.
                    let Some(editor) = (unsafe { state.cast::<$ty>().as_ref() }) else {
                        return;
                    };
                    let text = <$ty as $crate::author::Editor>::snapshot(editor);
                    sink(ctx, text.as_ptr(), text.len());
                }));
            }
            unsafe extern "C" fn cursor(state: *mut ::core::ffi::c_void) -> usize {
                ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    // SAFETY: the host serializes calls and keeps its handle alive for
                    // this one.
                    (unsafe { state.cast::<$ty>().as_ref() })
                        .map_or(0, |editor| <$ty as $crate::author::Editor>::cursor(editor))
                }))
                .unwrap_or(0)
            }
            unsafe extern "C" fn restore(
                state: *mut ::core::ffi::c_void,
                text: *const u8,
                len: usize,
                cursor: usize,
            ) -> i32 {
                ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    // SAFETY: the host serializes calls and keeps its handle alive for
                    // this one.
                    let Some(editor) = (unsafe { state.cast::<$ty>().as_mut() }) else {
                        return -1;
                    };
                    // SAFETY: the host lends the draft for this call, and the borrow ends
                    // with it.
                    let Some(text) = (unsafe { $crate::author::borrow_str(text, len) }) else {
                        return -1;
                    };
                    match <$ty as $crate::author::Editor>::restore(editor, text, cursor) {
                        Ok(()) => 0,
                        Err(_) => -1,
                    }
                }))
                .unwrap_or(-1)
            }
            unsafe extern "C" fn selected_text(
                state: *mut ::core::ffi::c_void,
                sink: $crate::ByteSink,
                ctx: *mut ::core::ffi::c_void,
            ) {
                let _ = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    // SAFETY: the host serializes calls and keeps its handle alive for
                    // this one.
                    let Some(editor) = (unsafe { state.cast::<$ty>().as_ref() }) else {
                        return;
                    };
                    let text = <$ty as $crate::author::Editor>::selected_text(editor);
                    sink(ctx, text.as_ptr(), text.len());
                }));
            }
            unsafe extern "C" fn transfer_state(
                destination: *mut ::core::ffi::c_void,
                source: *mut ::core::ffi::c_void,
            ) -> i32 {
                ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    if destination.is_null() || source.is_null() {
                        return -1;
                    }
                    if destination == source {
                        return 0;
                    }
                    // SAFETY: both handles were created by this library and are live and
                    // distinct for this serialized call.
                    let source = unsafe { &*source.cast::<$ty>() };
                    // SAFETY: the destination handle is live, distinct from the source,
                    // and exclusively borrowed for this serialized call.
                    let destination = unsafe { &mut *destination.cast::<$ty>() };
                    <$ty as $crate::author::Editor>::transfer_state(destination, source);
                    0
                }))
                .unwrap_or(-1)
            }
            static API: $crate::UiApi = $crate::UiApi {
                abi: $crate::ABI_VERSION,
                table_size: ::core::mem::size_of::<$crate::UiApi>() as u32,
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
            &API
        }
    };
    (renderer: $ty:ty $(,)?) => {
        // The exported entry point is named by the ABI, not by an author writing
        // documentation; the macro's own doc comment covers it.
        #[allow(missing_docs)]
        #[unsafe(no_mangle)]
        pub extern "C" fn eden_renderer_v1() -> *const $crate::RendererApi {
            unsafe extern "C" fn create() -> *mut ::core::ffi::c_void {
                ::std::panic::catch_unwind(<$ty as $crate::author::Renderer>::create)
                    .ok()
                    .flatten()
                    .map_or(::core::ptr::null_mut(), |renderer| {
                        ::std::boxed::Box::into_raw(::std::boxed::Box::new(renderer))
                            .cast::<::core::ffi::c_void>()
                    })
            }
            unsafe extern "C" fn destroy(state: *mut ::core::ffi::c_void) {
                let _ = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    if state.is_null() {
                        return;
                    }
                    // SAFETY: the host returns a handle this library allocated and
                    // releases it once; the null case returned above.
                    drop(unsafe { ::std::boxed::Box::from_raw(state.cast::<$ty>()) });
                }));
            }
            unsafe extern "C" fn render(
                state: *mut ::core::ffi::c_void,
                payload: *const u8,
                len: usize,
                width: u16,
                height: u16,
                sink: $crate::CellSink,
                ctx: *mut ::core::ffi::c_void,
            ) -> i32 {
                ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    // SAFETY: the host serializes calls and keeps its handle alive for
                    // this one.
                    let Some(renderer) = (unsafe { state.cast::<$ty>().as_mut() }) else {
                        return -1;
                    };
                    // SAFETY: the host lends the payload for this call, and the borrow
                    // ends with it.
                    let Some(payload) = (unsafe { $crate::author::borrow_bytes(payload, len) })
                    else {
                        return -1;
                    };
                    // SAFETY: the host lends its cell callback and context for this call,
                    // and the frame does not outlive them.
                    let mut frame =
                        unsafe { $crate::author::Frame::new(sink, ctx, width, height, 0, false) };
                    match <$ty as $crate::author::Renderer>::render(renderer, payload, &mut frame) {
                        Ok(()) => 0,
                        Err(_) => -1,
                    }
                }))
                .unwrap_or(-1)
            }
            static API: $crate::RendererApi = $crate::RendererApi {
                header: $crate::ApiHeader {
                    abi: $crate::ABI_VERSION,
                    table_size: ::core::mem::size_of::<$crate::RendererApi>() as u32,
                },
                create,
                destroy,
                render,
            };
            &API
        }
    };
    (theme: $ty:ty $(,)?) => {
        // The exported entry point is named by the ABI, not by an author writing
        // documentation; the macro's own doc comment covers it.
        #[allow(missing_docs)]
        #[unsafe(no_mangle)]
        pub extern "C" fn eden_theme_v1() -> *const $crate::ThemeApi {
            unsafe extern "C" fn resolve(dark: u32, token: u32) -> u32 {
                ::std::panic::catch_unwind(|| {
                    <$ty as $crate::author::Theme>::resolve(dark != 0, token)
                })
                .unwrap_or($crate::COLOR_DEFAULT)
            }
            static API: $crate::ThemeApi = $crate::ThemeApi {
                header: $crate::ApiHeader {
                    abi: $crate::ABI_VERSION,
                    table_size: ::core::mem::size_of::<$crate::ThemeApi>() as u32,
                },
                resolve,
            };
            &API
        }
    };
    (frontend: $ty:ty $(,)?) => {
        // The exported entry point is named by the ABI, not by an author writing
        // documentation; the macro's own doc comment covers it.
        #[allow(missing_docs)]
        #[unsafe(no_mangle)]
        pub extern "C" fn eden_terminal_frontend_v1() -> *const $crate::TerminalFrontendApi {
            unsafe extern "C" fn run(config: *const u8, len: usize) -> i32 {
                ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    // SAFETY: the host keeps the configuration alive until this call
                    // returns, and the borrow ends with it.
                    let Some(config) = (unsafe { $crate::author::borrow_bytes(config, len) })
                    else {
                        return $crate::author::FRONTEND_FAILURE_EXIT;
                    };
                    <$ty as $crate::author::Frontend>::run(config)
                }))
                .unwrap_or($crate::author::FRONTEND_FAILURE_EXIT)
            }
            static API: $crate::TerminalFrontendApi = $crate::TerminalFrontendApi {
                header: $crate::ApiHeader {
                    abi: $crate::ABI_VERSION,
                    table_size: ::core::mem::size_of::<$crate::TerminalFrontendApi>() as u32,
                },
                run,
            };
            &API
        }
    };
    (overlay: $ty:ty $(,)?) => {
        // The exported entry point is named by the ABI, not by an author writing
        // documentation; the macro's own doc comment covers it.
        #[allow(missing_docs)]
        #[unsafe(no_mangle)]
        pub extern "C" fn eden_overlay_v1() -> *const $crate::OverlayApi {
            unsafe extern "C" fn create() -> *mut ::core::ffi::c_void {
                ::std::panic::catch_unwind(<$ty as $crate::author::Overlay>::create)
                    .ok()
                    .flatten()
                    .map_or(::core::ptr::null_mut(), |overlay| {
                        ::std::boxed::Box::into_raw(::std::boxed::Box::new(overlay))
                            .cast::<::core::ffi::c_void>()
                    })
            }
            unsafe extern "C" fn destroy(state: *mut ::core::ffi::c_void) {
                let _ = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    if state.is_null() {
                        return;
                    }
                    // SAFETY: the host returns a handle this library allocated and
                    // releases it once; the null case returned above.
                    drop(unsafe { ::std::boxed::Box::from_raw(state.cast::<$ty>()) });
                }));
            }
            unsafe extern "C" fn render(
                state: *mut ::core::ffi::c_void,
                payload: *const u8,
                len: usize,
                width: u16,
                height: u16,
                sink: $crate::CellSink,
                ctx: *mut ::core::ffi::c_void,
            ) -> i32 {
                ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    // SAFETY: the host serializes calls and keeps its handle alive for
                    // this one.
                    let Some(overlay) = (unsafe { state.cast::<$ty>().as_mut() }) else {
                        return -1;
                    };
                    // SAFETY: the host lends the payload for this call, and the borrow
                    // ends with it.
                    let Some(payload) = (unsafe { $crate::author::borrow_bytes(payload, len) })
                    else {
                        return -1;
                    };
                    // SAFETY: the host lends its cell callback and context for this call,
                    // and the frame does not outlive them.
                    let mut frame =
                        unsafe { $crate::author::Frame::new(sink, ctx, width, height, 0, false) };
                    match <$ty as $crate::author::Overlay>::render(overlay, payload, &mut frame) {
                        Ok(()) => 0,
                        Err(_) => -1,
                    }
                }))
                .unwrap_or(-1)
            }
            unsafe extern "C" fn event(
                state: *mut ::core::ffi::c_void,
                payload: *const u8,
                len: usize,
                sink: $crate::ByteSink,
                ctx: *mut ::core::ffi::c_void,
            ) -> i32 {
                ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    // SAFETY: the host serializes calls and keeps its handle alive for
                    // this one.
                    let Some(overlay) = (unsafe { state.cast::<$ty>().as_mut() }) else {
                        return -1;
                    };
                    // SAFETY: the host lends the event for this call, and the borrow ends
                    // with it.
                    let Some(payload) = (unsafe { $crate::author::borrow_bytes(payload, len) })
                    else {
                        return -1;
                    };
                    match <$ty as $crate::author::Overlay>::event(overlay, payload) {
                        Ok(reply) => {
                            sink(ctx, reply.as_ptr(), reply.len());
                            0
                        }
                        Err(_) => -1,
                    }
                }))
                .unwrap_or(-1)
            }
            static API: $crate::OverlayApi = $crate::OverlayApi {
                renderer: $crate::RendererApi {
                    header: $crate::ApiHeader {
                        abi: $crate::ABI_VERSION,
                        table_size: ::core::mem::size_of::<$crate::OverlayApi>() as u32,
                    },
                    create,
                    destroy,
                    render,
                },
                event,
            };
            &API
        }
    };
}
