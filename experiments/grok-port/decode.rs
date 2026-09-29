//! Decode a captured PTY stream with the reference's own Alacritty terminal model.
use ptyctl::term::{ScreenOpts, SessionListener, Terminal};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("expected ANSI path"))?;
    let cols = args.next().unwrap_or_else(|| "110".into()).parse()?;
    let rows = args.next().unwrap_or_else(|| "40".into()).parse()?;
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut terminal = Terminal::new(cols, rows, SessionListener::new(tx));
    terminal.feed(&std::fs::read(path)?);
    let options = ScreenOpts {
        include_empty: true,
        ..Default::default()
    };
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "screen": terminal.screen_content(&options),
            "styled": terminal.screen_styled(&options),
        }))?
    );
    Ok(())
}
