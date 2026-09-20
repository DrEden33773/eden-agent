//! Native plugin host.

/// Declarative command surface of the `eden` executable.
pub mod cli;
/// Explicit environment-file parsing for CLI startup.
pub mod environment;
/// Shared-session management commands.
pub mod session_commands;
/// Single outlet for human-readable messages.
pub mod shell;
/// Terminal palette shared by clap's rendering and runtime messages.
pub mod style;
pub mod workspace_commands;

/// Workspace options implied by the shared command-line arguments.
pub fn workspace_options(cli: &cli::Cli) -> eden_agent::WorkspaceOptions {
    let mut options = eden_agent::WorkspaceOptions::default();
    if let Some(directory) = &cli.global_dir {
        options.global_dir = directory.clone();
    }
    if cli.trust_project {
        options.project_trust = Some(true);
    }
    if cli.no_trust_project {
        options.project_trust = Some(false);
    }
    options
}

/// Workspace options for the default run family.
///
/// Besides the shared arguments this family also carries the resource and tool
/// overrides, which the resource source reads back by their setting names.
pub fn prompt_options(cli: &cli::Cli) -> eden_agent::WorkspaceOptions {
    let mut options = workspace_options(cli);
    let overrides = &mut options.overrides;
    if !cli.tools.is_empty() {
        overrides["tools"] = serde_json::json!(names(&cli.tools));
    }
    if !cli.exclude_tools.is_empty() {
        overrides["exclude_tools"] = serde_json::json!(names(&cli.exclude_tools));
    }
    if !cli.skill_path.is_empty() {
        overrides["skills"] = serde_json::json!(cli.skill_path);
    }
    if !cli.template_path.is_empty() {
        overrides["templates"] = serde_json::json!(cli.template_path);
    }
    if cli.read_only {
        overrides["read_only"] = serde_json::json!(true);
    }
    for (disabled, setting) in [
        (cli.no_context, "discover_context"),
        (cli.no_skills, "discover_skills"),
        (cli.no_templates, "discover_templates"),
    ] {
        if disabled {
            overrides[setting] = serde_json::json!(false);
        }
    }
    options
}

/// Comma-separated tool names without the empty entries a trailing comma leaves.
fn names(values: &[String]) -> Vec<&str> {
    values
        .iter()
        .map(String::as_str)
        .filter(|name| !name.is_empty())
        .collect()
}

/// The composition this process runs: the explicit file, or the one beside the
/// installation that owns this executable.
pub fn composition(cli: &cli::Cli) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    Ok(match &cli.composition {
        Some(path) => path.clone(),
        None => std::env::current_exe()?
            .parent()
            .and_then(|directory| directory.parent())
            .ok_or("invalid installation layout")?
            .join("composition.json"),
    })
}

/// Wait for a settled run and optionally stream its ordered JSON events.
///
/// Human-readable diagnostics leave on stderr in both modes; `--json` only
/// promises that stdout stays a clean event stream.
pub async fn wait_for_run(
    session: &eden_agent::Session,
    run: u64,
    json: bool,
    shell: &shell::Shell,
) -> Result<eden_agent::Terminal, Box<dyn std::error::Error>> {
    for event in session
        .events()
        .iter()
        .filter(|event| event.kind == "resource_diagnostic")
    {
        shell.diagnostic(
            event.payload["message"]
                .as_str()
                .unwrap_or("resource diagnostic"),
        );
    }
    if !json {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    /// clap reads the program name from the first element, as a process does.
    fn parse(args: &[&str]) -> cli::Parsed {
        let mut line = vec![OsString::from("eden")];
        line.extend(args.iter().map(OsString::from));
        cli::parse(&line)
    }

    #[test]
    fn discovery_flags_disable_their_own_setting() {
        let parsed = parse(&["--no-skills", "--no-context", "prompt"]);
        let overrides = prompt_options(&parsed.cli).overrides;
        assert_eq!(overrides["discover_skills"], serde_json::json!(false));
        assert_eq!(overrides["discover_context"], serde_json::json!(false));
        assert!(overrides.get("discover_templates").is_none());
    }

    #[test]
    fn tool_names_keep_the_comma_list_shape() {
        let parsed = parse(&["--tools", "read,write,bash", "prompt"]);
        assert_eq!(
            prompt_options(&parsed.cli).overrides["tools"],
            serde_json::json!(["read", "write", "bash"])
        );
    }

    #[test]
    fn attachments_keep_command_line_order() {
        let parsed = parse(&[
            "--file", "a.pdf", "--attach", "b.txt", "--image", "c.png", "prompt",
        ]);
        let kinds: Vec<cli::AttachmentKind> = cli::attachments(&parsed)
            .into_iter()
            .map(|(kind, _)| kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                cli::AttachmentKind::File,
                cli::AttachmentKind::Text,
                cli::AttachmentKind::Image
            ]
        );
    }
}
