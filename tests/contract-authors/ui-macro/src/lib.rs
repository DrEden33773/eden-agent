//! Independent UI author written against the safe role traits and `export_ui!`.
//!
//! This library is the macro-authored counterpart of `tui-editor`: it exports the same
//! five roles with the same observable output — an `A` editor marker, an `AUTHOR
//! RENDERER` frame, an `AUTHOR OVERLAY` modal whose F9 closes it, the `12ab34` accent
//! and an `AUTHOR FRONTEND session` line — while containing no `unsafe`, no `extern "C"`
//! function, no table and no entry point of its own. Installed acceptance runs both
//! libraries through the same assertions.
use eden_ui_sdk::{
    COLOR_DEFAULT, author,
    author::{CellStyle, Frame, Rejected},
};

const MAX_TEXT: usize = 1024 * 1024;
const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
const ACCENT: u32 = 0x12_ab_34;
const MARKER: CellStyle = CellStyle {
    fg: ACCENT,
    bg: COLOR_DEFAULT,
    flags: 0,
};

fn accent(dark: bool, token: u32) -> u32 {
    let _ = dark;
    if token == eden_ui_sdk::theme_token::ACCENT {
        ACCENT
    } else {
        COLOR_DEFAULT
    }
}

mod marker {
    use super::*;

    /// Append-only draft with a visible marker, the smallest editor the host accepts.
    #[derive(Default)]
    struct Marker {
        text: String,
    }

    impl author::Editor for Marker {
        fn create(_style: u32) -> Option<Self> {
            Some(Self::default())
        }

        fn event(
            &mut self,
            kind: u32,
            key: u32,
            _modifiers: u32,
            text: &str,
        ) -> Result<(), Rejected> {
            if text.len() > MAX_TEXT {
                return Err(Rejected);
            }
            if kind == 1 || (kind == 0 && key == 10) {
                self.text.push_str(text);
            } else if kind == 0 && key == 7 {
                self.text.pop();
            }
            Ok(())
        }

        fn render(&mut self, frame: &mut Frame<'_>) {
            if frame.width() == 0 || frame.height() == 0 {
                return;
            }
            frame.set(0, 0, "A", CellStyle::default());
            frame.cursor(0, 0);
        }

        fn snapshot(&self) -> &str {
            &self.text
        }

        fn cursor(&self) -> usize {
            self.text.len()
        }

        fn restore(&mut self, text: &str, cursor: usize) -> Result<(), Rejected> {
            if cursor != text.len() || text.len() > MAX_TEXT {
                return Err(Rejected);
            }
            self.text = text.to_owned();
            Ok(())
        }

        fn transfer_state(&mut self, source: &Self) {
            self.text.clone_from(&source.text);
        }
    }

    eden_ui_sdk::export_ui!(editor: Marker);
}

mod messages {
    use super::*;

    /// Draws one fixed banner whenever the payload carries a message list.
    struct Messages;

    impl author::Renderer for Messages {
        fn create() -> Option<Self> {
            Some(Self)
        }

        fn render(&mut self, payload: &[u8], frame: &mut Frame<'_>) -> Result<(), Rejected> {
            if payload.len() > MAX_PAYLOAD {
                return Err(Rejected);
            }
            let value: serde_json::Value = serde_json::from_slice(payload).map_err(|_| Rejected)?;
            if value
                .get("messages")
                .and_then(serde_json::Value::as_array)
                .is_none()
            {
                return Err(Rejected);
            }
            if frame.width() != 0 && frame.height() != 0 {
                frame.set(0, 0, "AUTHOR RENDERER", MARKER);
            }
            if value["fail_after_draw"].as_bool() == Some(true) {
                return Err(Rejected);
            }
            Ok(())
        }
    }

    eden_ui_sdk::export_ui!(renderer: Messages);
}

mod accent_palette {
    use super::*;

    struct Accent;

    impl author::Theme for Accent {
        fn resolve(dark: bool, token: u32) -> u32 {
            accent(dark, token)
        }
    }

    eden_ui_sdk::export_ui!(theme: Accent);
}

mod session {
    use super::*;

    /// Attaches, reads one snapshot, detaches and prints the session it saw.
    struct Session;

    impl author::Frontend for Session {
        fn run(config: &[u8]) -> i32 {
            if config.len() > MAX_TEXT {
                return 2;
            }
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(config) else {
                return 2;
            };
            let Some(endpoint) = value["endpoint"].as_str().map(str::to_owned) else {
                return 2;
            };
            // The role owns its runtime in a separate thread, so the host may already run
            // Tokio, and it joins that thread before returning across the boundary.
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
    }

    eden_ui_sdk::export_ui!(frontend: Session);
}

mod dialogue {
    use super::*;

    /// Modal whose F9 mapping closes the dialog and whose other keys pass through.
    struct Dialogue;

    impl author::Overlay for Dialogue {
        fn create() -> Option<Self> {
            Some(Self)
        }

        fn render(&mut self, payload: &[u8], frame: &mut Frame<'_>) -> Result<(), Rejected> {
            if payload.len() > MAX_PAYLOAD {
                return Err(Rejected);
            }
            let value: serde_json::Value = serde_json::from_slice(payload).map_err(|_| Rejected)?;
            if !value["kind"].is_string() || !value["theme"].is_object() {
                return Err(Rejected);
            }
            if frame.width() > 0 && frame.height() > 0 {
                frame.set(0, 0, "AUTHOR OVERLAY", MARKER);
            }
            Ok(())
        }

        fn event(&mut self, event: &[u8]) -> Result<&str, Rejected> {
            let value: serde_json::Value = serde_json::from_slice(event).map_err(|_| Rejected)?;
            if value["key"] == "F(9)" {
                Ok("\"close\"")
            } else {
                Ok("\"pass\"")
            }
        }
    }

    eden_ui_sdk::export_ui!(overlay: Dialogue);
}
