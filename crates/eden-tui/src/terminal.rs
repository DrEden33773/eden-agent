//! Terminal modes have one scoped owner, including editor and suspend handoffs.
use crate::{
    app::{App, Update},
    input, view,
};
use crossterm::{
    cursor,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use eden_tui_client::HostClient;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// Local terminal preferences are independent of shared business configuration.
#[derive(Clone, Debug)]
pub struct Options {
    /// Endpoint selected by the caller; never discovered implicitly.
    pub endpoint: PathBuf,
    /// Editor library chosen from the installation or an explicit replacement.
    pub editor: PathBuf,
    /// Per-user local storage, containing ordinary composer drafts only.
    pub state_dir: PathBuf,
    /// Stable local frontend identity; simultaneous clients should use distinct values.
    pub frontend: String,
    /// Disable colored text while keeping semantic layout.
    pub no_color: bool,
}
struct Modes {
    mouse: bool,
}
impl Modes {
    fn enter(mouse: bool) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let owner = Self { mouse };
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        if mouse {
            execute!(io::stdout(), EnableMouseCapture)?;
        }
        Ok(owner)
    }
    fn mouse(&mut self, on: bool) -> io::Result<()> {
        if on != self.mouse {
            if on {
                execute!(io::stdout(), EnableMouseCapture)?
            } else {
                execute!(io::stdout(), DisableMouseCapture)?
            }
            self.mouse = on;
        }
        Ok(())
    }
}
fn restore() {
    let _ = execute!(
        io::stdout(),
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableMouseCapture,
        cursor::Show,
        LeaveAlternateScreen
    );
    let _ = terminal::disable_raw_mode();
}
impl Drop for Modes {
    fn drop(&mut self) {
        restore();
    }
}
struct Follower(tokio::task::JoinHandle<()>);
impl Drop for Follower {
    fn drop(&mut self) {
        self.0.abort();
    }
}
/// Attach one terminal to the explicit host. Detaching leaves accepted work running.
pub async fn run(mut options: Options) -> Result<i32, Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(
            "interactive frontend requires terminal stdin and stdout; use --print or --json".into(),
        );
    }
    if let Some(path) = std::env::var_os("EDEN_TUI_FRONTEND") {
        let mut frontend = crate::extensions::Frontend::load(Path::new(&path))?;
        return frontend.run(&serde_json::json!({
            "endpoint": options.endpoint,
            "frontend": options.frontend,
            "editor": options.editor,
            "state_dir": options.state_dir,
            "no_color": options.no_color,
            "session": null,
            "plugins": {
                "editor": options.editor,
                "renderer": std::env::var_os("EDEN_TUI_RENDERER"),
                "theme": std::env::var_os("EDEN_TUI_THEME"),
                "overlay": std::env::var_os("EDEN_TUI_OVERLAY"),
            },
        }));
    }
    let prior = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        prior(info)
    }));
    loop {
        let identity = HostClient::new(&options.endpoint)
            .snapshot()
            .await?
            .presentation
            .session_id;
        let client = HostClient::for_session(&options.endpoint, identity);
        let lease = Arc::new(AtomicU64::new(client.attach("tui").await?));
        let result = run_attached(&options, client.clone(), lease.clone()).await;
        let detached = client.detach(lease.load(Ordering::Relaxed)).await;
        if options.endpoint.exists() {
            detached?;
        }
        let (code, next) = result?;
        if let Some(endpoint) = next {
            options.endpoint = endpoint;
            continue;
        }
        if options.endpoint.exists() {
            println!(
                "Resume: eden tui --endpoint {} --frontend {} --editor {}",
                shell_word(&options.endpoint.to_string_lossy()),
                shell_word(&options.frontend),
                shell_word(&options.editor.to_string_lossy())
            );
        }
        return Ok(code);
    }
}

fn shell_word(value: &str) -> String {
    if cfg!(windows) {
        format!("'{}'", value.replace('\'', "''"))
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

async fn run_attached(
    options: &Options,
    client: HostClient,
    lease: Arc<AtomicU64>,
) -> Result<(i32, Option<PathBuf>), Box<dyn std::error::Error>> {
    let snapshot = client.snapshot().await?;
    let client = HostClient::for_session(&options.endpoint, snapshot.presentation.session_id);
    let (tx, rx) = mpsc::channel();
    let updates = tx.clone();
    let ui_lease = lease.clone();
    let frontend = "tui".to_owned();
    let mut current = snapshot.clone();
    let _follower = Follower(tokio::spawn(async move {
        let mut projection = crate::app::Projection::new();
        loop {
            match client
                .poll_incremental(lease.load(Ordering::Relaxed), current.clone())
                .await
            {
                Ok(snapshot) => {
                    current = snapshot.clone();
                    if updates
                        .send(Update::Snapshot(
                            Box::new(snapshot.clone()),
                            projection.update(&snapshot),
                        ))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    if updates
                        .send(Update::Connection(format!(
                            "Disconnected · {error} · reconnecting"
                        )))
                        .is_err()
                    {
                        break;
                    }
                    if (error.code == "AttachmentExpired"
                        || (error.code == "InvalidInput" && error.message.contains("attachment")))
                        && let Ok(attachment) = client.attach(&frontend).await
                    {
                        lease.store(attachment, Ordering::Relaxed);
                        if let Ok(snapshot) = client.snapshot().await {
                            current = snapshot;
                        }
                        continue;
                    }
                    if updates
                        .send(Update::Connection(format!(
                            "Disconnected · {error} · reconnecting"
                        )))
                        .is_err()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }));
    tokio::task::block_in_place(|| {
        let mut app = App::new(
            &options.editor,
            Some(&options.state_dir),
            &options.frontend,
            &options.endpoint,
            snapshot,
            tx,
            rx,
        )?;
        app.lease = Some(ui_lease);
        let preferences = options.state_dir.join("ui.json");
        if let Ok(bytes) = std::fs::read(&preferences)
            && let Ok(value) = serde_json::from_slice::<crate::model::Preferences>(&bytes)
        {
            if value.valid() {
                app.preferences = value;
            } else {
                app.notice = "Invalid UI settings · previous preferences retained".into();
            }
        }
        let result = drive(&mut app, options, &preferences);
        app.cancel_management_scan();
        app.release_management_preview();
        app.saved = false;
        app.persist();
        result
    })
}
fn drive(
    app: &mut App,
    options: &Options,
    preferences: &Path,
) -> Result<(i32, Option<PathBuf>), Box<dyn std::error::Error>> {
    let mut modes = Some(Modes::enter(app.preferences.mouse)?);
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut geometry = view::Geometry::default();
    let mut kitty = crate::image_preview::Kitty::default();
    let mut pointer = input::Pointer::default();
    let mut dirty = true;
    let mut frame = Instant::now();
    let mut theme_checked = Instant::now();
    let mut last_preferences = app.preferences.clone();
    loop {
        dirty |= app.tick();
        if app.phase != crate::model::Phase::Idle
            && app.preferences.motion
            && frame.elapsed() >= Duration::from_millis(80)
        {
            app.animation_frame += 1;
            dirty = true;
        }
        if theme_checked.elapsed() >= Duration::from_secs(1) {
            theme_checked = Instant::now();
            if let Ok(bytes) = std::fs::read(preferences)
                && let Ok(value) = serde_json::from_slice::<crate::model::Preferences>(&bytes)
                && app.preferences == last_preferences
            {
                if value.valid() {
                    app.preferences = value;
                } else {
                    app.notice = "Invalid UI settings · previous preferences retained".into();
                }
                dirty = true;
            }
        }
        if app.preferences != last_preferences {
            std::fs::create_dir_all(&options.state_dir)?;
            std::fs::write(preferences, serde_json::to_vec_pretty(&app.preferences)?)?;
            last_preferences = app.preferences.clone();
            if let Some(owner) = &mut modes {
                owner.mouse(app.preferences.mouse)?;
            }
            dirty = true;
        }
        if dirty {
            terminal.draw(|f| {
                let area = f.area();
                let (g, cursor) = view::render(app, f.buffer_mut(), area, options.no_color);
                geometry = g;
                if let Some(position) = cursor {
                    f.set_cursor_position(position);
                }
            })?;
            kitty.draw(geometry.image.as_ref(), &mut io::stdout())?;
            dirty = false;
            frame = Instant::now();
        }
        if app.quit {
            break;
        }
        app.clipboard_worker();
        if let Some(sequence) = app.clipboard_escape.take() {
            io::stdout().write_all(sequence.as_bytes())?;
            io::stdout().flush()?;
            app.notice = "OSC52 copy requested · terminal confirmation unavailable".into();
            dirty = true;
        }
        if app.external_editor || app.suspend {
            kitty.clear(&mut io::stdout())?;
            drop(modes.take());
            if app.external_editor {
                app.external_editor = false;
                external_editor(app)?;
            }
            if app.suspend {
                app.suspend = false;
                suspend()?;
            }
            modes = Some(Modes::enter(app.preferences.mouse)?);
            terminal.clear()?;
            dirty = true;
            continue;
        }
        // Drain a burst before drawing: pasted/queued keystrokes must not pay one full
        // streamed-text layout per byte. Keep a bound so model updates still make progress.
        for index in 0..64 {
            if !event::poll(if index == 0 {
                Duration::from_millis(16)
            } else {
                Duration::ZERO
            })? {
                break;
            }
            match event::read()? {
                Event::Key(key) => {
                    app.key(key);
                    dirty = true
                }
                Event::Paste(text) => {
                    app.paste(&text);
                    dirty = true
                }
                Event::Mouse(mouse) => {
                    if matches!(
                        mouse.kind,
                        event::MouseEventKind::Down(_) | event::MouseEventKind::Drag(_)
                    ) {
                        app.clipboard_generation += 1;
                    }
                    dirty |= input::mouse(app, &geometry, mouse, &mut pointer);
                }
                Event::Resize(..) => {
                    kitty.clear(&mut io::stdout())?;
                    terminal.clear()?;
                    dirty = true
                }
                _ => {}
            }
        }
    }
    kitty.clear(&mut io::stdout())?;
    drop(modes);
    Ok((0, app.switch_endpoint.take()))
}
fn external_editor(app: &mut App) -> Result<(), Box<dyn std::error::Error>> {
    let original = app.draft();
    let path = std::env::temp_dir().join(format!(
        "eden-editor-{}-{}.txt",
        std::process::id(),
        app.session
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    file.write_all(original.text.as_bytes())?;
    drop(file);
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                "notepad".into()
            } else {
                "vi".into()
            }
        });
    let result = std::process::Command::new(editor).arg(&path).status();
    match result {
        Ok(status) if status.success() => match std::fs::read_to_string(&path) {
            Ok(text) => {
                let _ = app.editor.restore(&text, text.len());
                app.changed();
            }
            Err(error) => {
                app.notice = format!("Editor output unreadable · original draft retained: {error}")
            }
        },
        Ok(status) => app.notice = format!("Editor exited {status} · original draft retained"),
        Err(error) => app.notice = format!("Editor failed · original draft retained: {error}"),
    }
    let _ = std::fs::remove_file(path);
    Ok(())
}
fn suspend() -> io::Result<()> {
    #[cfg(unix)]
    {
        let status = std::process::Command::new("kill")
            .args(["-STOP", &std::process::id().to_string()])
            .status()?;
        if !status.success() {
            return Err(io::Error::other("suspend failed"));
        }
    }
    Ok(())
}
