//! Native plugin host.

/// Explicit environment-file parsing for CLI startup.
pub mod environment;

/// Shared-session management commands.
pub mod session_commands;
pub mod workspace_commands;

/// Wait for a settled run and optionally stream its ordered JSON events.
pub async fn wait_for_run(
    session: &eden_agent::Session,
    run: u64,
    json: bool,
) -> Result<eden_agent::Terminal, Box<dyn std::error::Error>> {
    if !json {
        for event in session
            .events()
            .iter()
            .filter(|event| event.kind == "resource_diagnostic")
        {
            eprintln!(
                "{}",
                event.payload["message"]
                    .as_str()
                    .unwrap_or("resource diagnostic")
            );
        }
        return Ok(session.wait(run).await?);
    }
    use std::io::Write;
    let mut sequence = 0;
    loop {
        let events = session.events_after(sequence).await;
        let mut settled = false;
        for event in events {
            sequence = event.sequence;
            settled |= event.kind == "settled" && event.run_id == run;
            let mut stdout = std::io::stdout().lock();
            serde_json::to_writer(&mut stdout, &event)?;
            writeln!(stdout)?;
            stdout.flush()?;
        }
        if settled {
            return Ok(session.wait(run).await?);
        }
    }
}
