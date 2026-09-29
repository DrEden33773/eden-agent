//! Independent editor author: no host, terminal framework or official editor dependency.
//! Intentionally supports append, paste, backspace, snapshots and a visible author marker.
//! Hosts select this library through the normal editor plugin configuration.
use std::ffi::c_void;

use eden_ui_sdk::{
    ABI_VERSION, ByteSink, CELL_CURSOR, COLOR_DEFAULT, Cell, CellSink, EVENT_ERROR, UiApi,
};

unsafe extern "C" fn create(_: u32) -> *mut c_void {
    Box::into_raw(Box::new(String::new())).cast()
}
unsafe extern "C" fn destroy(handle: *mut c_void) {
    if !handle.is_null() {
        // SAFETY: the SDK caller releases this library's unique create handle once.
        drop(unsafe { Box::from_raw(handle.cast::<String>()) });
    }
}
unsafe extern "C" fn event(
    handle: *mut c_void,
    kind: u32,
    key: u32,
    _: u32,
    text: *const u8,
    len: usize,
) -> u32 {
    // SAFETY: the caller serializes access to a live handle created here.
    let Some(value) = (unsafe { handle.cast::<String>().as_mut() }) else {
        return EVENT_ERROR;
    };
    if kind == 1 || (kind == 0 && key == 10) {
        if len > 1024 * 1024 || (len != 0 && text.is_null()) {
            return EVENT_ERROR;
        }
        let bytes = if len == 0 {
            &[]
        } else {
            // SAFETY: input bytes are borrowed for this call by the SDK contract.
            unsafe { std::slice::from_raw_parts(text, len) }
        };
        let Ok(text) = std::str::from_utf8(bytes) else {
            return EVENT_ERROR;
        };
        value.push_str(text);
    } else if kind == 0 && key == 7 {
        value.pop();
    }
    0
}
unsafe extern "C" fn snapshot(handle: *mut c_void, sink: ByteSink, ctx: *mut c_void) {
    // SAFETY: borrowed only during this serialized call; sink must copy synchronously.
    if let Some(value) = unsafe { handle.cast::<String>().as_ref() } {
        sink(ctx, value.as_ptr(), value.len());
    }
}
unsafe extern "C" fn cursor(handle: *mut c_void) -> usize {
    // SAFETY: the SDK caller keeps this library's handle alive.
    unsafe { handle.cast::<String>().as_ref() }.map_or(0, String::len)
}
unsafe extern "C" fn restore(
    handle: *mut c_void,
    text: *const u8,
    len: usize,
    offset: usize,
) -> i32 {
    if offset != len || len > 1024 * 1024 || (len != 0 && text.is_null()) {
        return -1;
    }
    let bytes = if len == 0 {
        &[]
    } else {
        // SAFETY: input is borrowed for the call; no data survives except the owned copy.
        unsafe { std::slice::from_raw_parts(text, len) }
    };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return -1;
    };
    // SAFETY: serialized SDK handle access.
    let Some(value) = (unsafe { handle.cast::<String>().as_mut() }) else {
        return -1;
    };
    *value = text.to_owned();
    0
}
unsafe extern "C" fn selected_text(_: *mut c_void, sink: ByteSink, ctx: *mut c_void) {
    sink(ctx, b"".as_ptr(), 0);
}
unsafe extern "C" fn transfer_state(destination: *mut c_void, source: *mut c_void) -> i32 {
    if source == destination {
        return 0;
    }
    // SAFETY: distinct live handles from this library, borrowed for this call only.
    let Some(source) = (unsafe { source.cast::<String>().as_ref() }) else {
        return -1;
    };
    // SAFETY: distinct live destination; the caller serializes access.
    let Some(destination) = (unsafe { destination.cast::<String>().as_mut() }) else {
        return -1;
    };
    destination.clone_from(source);
    0
}
unsafe extern "C" fn render(
    _: *mut c_void,
    width: u16,
    height: u16,
    _: u32,
    _: u32,
    sink: CellSink,
    ctx: *mut c_void,
) {
    if width == 0 || height == 0 {
        return;
    }
    let marker = b"A";
    sink(
        ctx,
        &Cell {
            x: 0,
            y: 0,
            fg: COLOR_DEFAULT,
            bg: COLOR_DEFAULT,
            flags: 0,
            text: marker.as_ptr(),
            text_len: 1,
        },
    );
    sink(
        ctx,
        &Cell {
            x: 0,
            y: 0,
            fg: COLOR_DEFAULT,
            bg: COLOR_DEFAULT,
            flags: CELL_CURSOR,
            text: marker.as_ptr(),
            text_len: 0,
        },
    );
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
/// The table remains valid until this library is unloaded, after all handles are destroyed.
#[unsafe(no_mangle)]
pub extern "C" fn eden_ui_v1() -> *const UiApi {
    &API
}

unsafe extern "C" fn renderer_create() -> *mut c_void {
    Box::into_raw(Box::new(())).cast()
}
unsafe extern "C" fn renderer_destroy(state: *mut c_void) {
    if !state.is_null() {
        // SAFETY: this fixture's renderer_create allocated the unique handle.
        drop(unsafe { Box::from_raw(state.cast::<()>()) });
    }
}
unsafe extern "C" fn renderer_render(
    _: *mut c_void,
    bytes: *const u8,
    len: usize,
    width: u16,
    height: u16,
    sink: CellSink,
    ctx: *mut c_void,
) -> i32 {
    if len == 0 || len > 16 * 1024 * 1024 || bytes.is_null() {
        return -1;
    }
    // SAFETY: SDK input bytes are borrowed and readable for this call.
    let bytes = unsafe { std::slice::from_raw_parts(bytes, len) };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return -1;
    };
    if value
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .is_none()
    {
        return -1;
    }
    if width != 0 && height != 0 {
        let marker = b"AUTHOR RENDERER";
        sink(
            ctx,
            &Cell {
                x: 0,
                y: 0,
                fg: 0x12ab34,
                bg: COLOR_DEFAULT,
                flags: 0,
                text: marker.as_ptr(),
                text_len: marker.len(),
            },
        );
    }
    if value["fail_after_draw"].as_bool() == Some(true) {
        -1
    } else {
        0
    }
}
unsafe extern "C" fn theme_resolve(_: u32, token: u32) -> u32 {
    if token == eden_ui_sdk::theme_token::ACCENT {
        0x12ab34
    } else {
        COLOR_DEFAULT
    }
}
unsafe extern "C" fn frontend_run(bytes: *const u8, len: usize) -> i32 {
    if len == 0 || len > 1024 * 1024 || bytes.is_null() {
        return 2;
    }
    // SAFETY: the host keeps serialized configuration live until run returns.
    let bytes = unsafe { std::slice::from_raw_parts(bytes, len) };
    let Ok(config) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return 2;
    };
    let Some(endpoint) = config["endpoint"].as_str().map(str::to_owned) else {
        return 2;
    };
    // The plugin owns its runtime in a separate thread, so the host may already run Tokio.
    let worker = std::thread::Builder::new().spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return 3;
        };
        runtime.block_on(async move {
            let client = eden_tui_client::HostClient::new(endpoint);
            let Ok(attachment) = client.attach("tui").await else {
                return 4;
            };
            let snapshot = client.snapshot().await;
            let detached = client.detach(attachment).await;
            match (snapshot, detached) {
                (Ok(snapshot), Ok(())) => {
                    use std::io::Write;
                    let _ = writeln!(
                        std::io::stdout().lock(),
                        "AUTHOR FRONTEND session {}",
                        snapshot.state.session_id
                    );
                    0
                }
                _ => 5,
            }
        })
    });
    worker.map_or(6, |worker| worker.join().unwrap_or(6))
}
static RENDERER: eden_ui_sdk::RendererApi = eden_ui_sdk::RendererApi {
    header: eden_ui_sdk::ApiHeader {
        abi: ABI_VERSION,
        table_size: std::mem::size_of::<eden_ui_sdk::RendererApi>() as u32,
    },
    create: renderer_create,
    destroy: renderer_destroy,
    render: renderer_render,
};
static THEME: eden_ui_sdk::ThemeApi = eden_ui_sdk::ThemeApi {
    header: eden_ui_sdk::ApiHeader {
        abi: ABI_VERSION,
        table_size: std::mem::size_of::<eden_ui_sdk::ThemeApi>() as u32,
    },
    resolve: theme_resolve,
};
static FRONTEND: eden_ui_sdk::TerminalFrontendApi = eden_ui_sdk::TerminalFrontendApi {
    header: eden_ui_sdk::ApiHeader {
        abi: ABI_VERSION,
        table_size: std::mem::size_of::<eden_ui_sdk::TerminalFrontendApi>() as u32,
    },
    run: frontend_run,
};
/// Demonstrates a independently compiled message renderer with default terminal background.
#[unsafe(no_mangle)]
pub extern "C" fn eden_renderer_v1() -> *const eden_ui_sdk::RendererApi {
    &RENDERER
}
/// A distinctive accent lets installed acceptance distinguish this palette from defaults.
#[unsafe(no_mangle)]
pub extern "C" fn eden_theme_v1() -> *const eden_ui_sdk::ThemeApi {
    &THEME
}
/// This minimal frontend attaches, reads and detaches through the public host client.
#[unsafe(no_mangle)]
pub extern "C" fn eden_terminal_frontend_v1() -> *const eden_ui_sdk::TerminalFrontendApi {
    &FRONTEND
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" fn receive(ctx: *mut c_void, bytes: *const u8, len: usize) {
        // SAFETY: the test passes a live Vec and the SDK borrows valid callback bytes.
        unsafe {
            let output = &mut *ctx.cast::<Vec<u8>>();
            output.extend_from_slice(std::slice::from_raw_parts(bytes, len));
        }
    }

    #[test]
    fn author_accepts_input_and_returns_owned_snapshot_to_host() {
        let mut output = Vec::<u8>::new();
        // SAFETY: use this library's table and live handle serially; destroy before return.
        unsafe {
            let api = &*eden_ui_v1();
            let handle = (api.create)(0);
            assert!(!handle.is_null());
            assert_eq!(
                (api.event)(handle, 1, 0, 0, b"external author".as_ptr(), 15),
                0
            );
            (api.snapshot)(handle, receive, (&mut output as *mut Vec<u8>).cast());
            assert_eq!(output, b"external author");
            (api.destroy)(handle);
        }
    }
}

unsafe extern "C" fn overlay_render(
    _: *mut c_void,
    bytes: *const u8,
    len: usize,
    width: u16,
    height: u16,
    sink: CellSink,
    ctx: *mut c_void,
) -> i32 {
    if bytes.is_null() || len == 0 || len > 16 * 1024 * 1024 {
        return -1;
    }
    // SAFETY: the caller lends JSON bytes for this synchronous call.
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(unsafe {
        std::slice::from_raw_parts(bytes, len)
    }) else {
        return -1;
    };
    if !value["kind"].is_string() || !value["theme"].is_object() {
        return -1;
    }
    if width > 0 && height > 0 {
        let marker = b"AUTHOR OVERLAY";
        sink(
            ctx,
            &Cell {
                x: 0,
                y: 0,
                fg: 0x12ab34,
                bg: COLOR_DEFAULT,
                flags: 0,
                text: marker.as_ptr(),
                text_len: marker.len(),
            },
        );
    }
    0
}
unsafe extern "C" fn overlay_event(
    _: *mut c_void,
    bytes: *const u8,
    len: usize,
    sink: ByteSink,
    ctx: *mut c_void,
) -> i32 {
    if bytes.is_null() || len == 0 || len > 4096 {
        return -1;
    }
    // SAFETY: SDK event bytes are borrowed only for this call.
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(unsafe {
        std::slice::from_raw_parts(bytes, len)
    }) else {
        return -1;
    };
    let reply: &[u8] = if value["key"] == "F(9)" {
        b"\"close\""
    } else {
        b"\"pass\""
    };
    sink(ctx, reply.as_ptr(), reply.len());
    0
}
static OVERLAY: eden_ui_sdk::OverlayApi = eden_ui_sdk::OverlayApi {
    renderer: eden_ui_sdk::RendererApi {
        header: eden_ui_sdk::ApiHeader {
            abi: ABI_VERSION,
            table_size: std::mem::size_of::<eden_ui_sdk::OverlayApi>() as u32,
        },
        create: renderer_create,
        destroy: renderer_destroy,
        render: overlay_render,
    },
    event: overlay_event,
};
/// An independently linked modal renderer whose F9 mapping closes the current dialog.
#[unsafe(no_mangle)]
pub extern "C" fn eden_overlay_v1() -> *const eden_ui_sdk::OverlayApi {
    &OVERLAY
}
