//! Noninteractive history and session commands using the shared Session API.
use crate::cli::{Cli, CopyArgs, Family, HistoryAction, SessionAction};
use crate::shell::Shell;
use eden_agent::{CopyKind, CopyOptions, Outcome, Session};
use eden_protocol::coding::Block;
use serde_json::json;
use std::{
    error::Error,
    io::Write,
    path::{Path, PathBuf},
};
type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Run the `history` and `session` families.
pub async fn run(cli: &Cli, shell: &Shell) -> Result<i32> {
    let action = match cli.family.as_ref() {
        Some(Family::History { action }) => {
            return match action {
                HistoryAction::Inspect { path } => inspect(path, shell),
                HistoryAction::Export { path, destination } => export(path, destination),
            };
        }
        Some(Family::Session { action }) => action,
        _ => return Err("session command required".into()),
    };
    match action {
        SessionAction::Info { path } => return info(path, shell),
        SessionAction::Tree { path } => return tree(path, shell),
        _ => {}
    }
    if let Some((kind, arguments, target, public_only)) = copy_action(action) {
        return copy_session(cli, kind, arguments, target, public_only).await;
    }
    control(cli, shell, action).await
}

/// The copy kind a session action performs, if it copies a session.
fn copy_action(action: &SessionAction) -> Option<(CopyKind, &CopyArgs, Option<u64>, bool)> {
    match action {
        SessionAction::Fork { copy, at } => Some((CopyKind::Fork, copy, *at, false)),
        SessionAction::Clone { copy } => Some((CopyKind::Clone, copy, None, false)),
        SessionAction::Import { copy } => Some((CopyKind::Import, copy, None, false)),
        SessionAction::Upgrade { copy } => Some((CopyKind::Upgrade, copy, None, false)),
        SessionAction::Recover { copy } => Some((CopyKind::Recover, copy, None, false)),
        SessionAction::Migrate { copy, public_only } => {
            Some((CopyKind::Migrate, copy, None, *public_only))
        }
        _ => None,
    }
}

/// Print every stored record; a damaged tail is reported and still exits 2.
fn inspect(path: &Path, shell: &Shell) -> Result<i32> {
    let scan = eden_kernel::history::inspect(path)?;
    for record in &scan.records {
        writeln!(std::io::stdout(), "{}", serde_json::to_string(record)?)?;
    }
    if let Some(diagnostic) = scan.diagnostic {
        shell.error(diagnostic);
        return Ok(2);
    }
    Ok(0)
}

fn export(path: &Path, destination: &Path) -> Result<i32> {
    let scan = eden_kernel::history::inspect(path)?;
    if let Some(error) = scan.diagnostic {
        return Err(error.into());
    }
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    // Keep the public physical framing so imports can validate atomic commits.
    if scan.records.first().is_some_and(|r| r.schema_version == 2) {
        output.write_all(&eden_protocol::history::encode_transaction(&scan.records)?)?;
    } else {
        for record in &scan.records {
            serde_json::to_writer(&mut output, record)?;
            output.write_all(b"\n")?;
        }
    }
    output.sync_all()?;
    Ok(0)
}

fn info(path: &Path, shell: &Shell) -> Result<i32> {
    let scan = eden_kernel::history::inspect(path)?;
    let state = eden_protocol::history::branch_state(&scan.records)?;
    let metadata = scan
        .records
        .iter()
        .rev()
        .find(|r| r.kind == "session_metadata")
        .map(|r| &r.payload);
    writeln!(
        std::io::stdout(),
        "{}",
        json!({
            "session_id": scan.records.first().map(|r| r.session_id),
            "records": scan.records.len(),
            "head": state.0,
            "branch": state.1,
            "binding": scan.records.first().map(|r| &r.payload),
            "metadata": metadata,
            "diagnostic": scan.diagnostic,
        })
    )?;
    if let Some(diagnostic) = scan.diagnostic {
        shell.error(diagnostic);
        return Ok(2);
    }
    Ok(0)
}

fn tree(path: &Path, shell: &Shell) -> Result<i32> {
    let scan = eden_kernel::history::inspect(path)?;
    for record in &scan.records {
        writeln!(
            std::io::stdout(),
            "{}",
            json!({
                "id": record.sequence,
                "parent": record.parent_id,
                "branch": record.branch,
                "kind": record.kind,
            })
        )?;
    }
    if let Some(diagnostic) = scan.diagnostic {
        shell.error(diagnostic);
        return Ok(2);
    }
    Ok(0)
}

async fn copy_session(
    cli: &Cli,
    kind: CopyKind,
    copy: &CopyArgs,
    target: Option<u64>,
    public_only: bool,
) -> Result<i32> {
    let plan = Session::plan_copy(
        &crate::composition(cli)?,
        CopyOptions {
            source: copy.source.clone(),
            destination: copy.destination.clone(),
            kind,
            target,
            cwd: cli.cwd.clone(),
            public_only,
        },
    )
    .await?;
    writeln!(std::io::stdout(), "{}", serde_json::to_string(&plan)?)?;
    if copy.apply {
        writeln!(
            std::io::stdout(),
            "{}",
            json!({ "created": Session::apply_copy(plan).await? })
        )?;
    }
    Ok(0)
}

async fn control(cli: &Cli, shell: &Shell, action: &SessionAction) -> Result<i32> {
    let json_output = cli.json;
    let path = action_path(action).ok_or("session action needs a history path")?;
    let options = crate::session_options(cli, Some(path), false)?;
    let workspace = crate::workspace_options(cli);
    let session = if matches!(action, SessionAction::Switch { .. }) {
        Session::open_rebound(&crate::composition(cli)?, options, workspace).await?
    } else {
        Session::open_with_workspace(&crate::composition(cli)?, options, workspace).await?
    };
    let result = async {
        if matches!(action, SessionAction::Switch { .. }) {
            writeln!(
                std::io::stdout(),
                "{{\"available\":true,\"binding_updated\":true}}"
            )?;
            return Ok(0);
        }
        if matches!(action, SessionAction::Resources { .. }) {
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string(&session.resources().await?)?
            )?;
            return Ok(0);
        }
        if matches!(action, SessionAction::Queue { .. }) {
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string(&session.queued().await?)?
            )?;
            return Ok(0);
        }
        if let SessionAction::Enqueue { text, kind, .. } = action {
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string(
                    &session
                        .enqueue(kind, vec![Block::Text { text: text.clone() }])
                        .await?
                )?
            )?;
            return Ok(0);
        }
        let run = match action {
            SessionAction::Branch {
                at,
                branch,
                summarize,
                ..
            } => session.navigate(*at, branch.clone(), *summarize)?,
            SessionAction::Compact { instructions, .. } => {
                session.compact(instructions.clone().unwrap_or_default())?
            }
            SessionAction::Metadata { name, tag, .. } => {
                session.set_metadata(name.clone(), tag.clone())?
            }
            SessionAction::QueueMode {
                steering,
                follow_up,
                ..
            } => session.configure_queue(steering.clone(), follow_up.clone())?,
            SessionAction::IncludeAttachment { at, .. } => session.include_attachment(*at)?,
            SessionAction::Continue { .. } => session.resume()?,
            _ => return Err("unknown session command".into()),
        };
        let cancel_session = session.clone();
        let signal = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = cancel_session.cancel(run);
            }
        });
        let terminal = crate::wait_for_run(&session, run, json_output, shell).await;
        signal.abort();
        let terminal = terminal?;
        if !json_output {
            writeln!(std::io::stdout(), "{}", serde_json::to_string(&terminal)?)?;
        }
        if !terminal.cleanup_errors.is_empty() {
            return Err("operation cleanup failed; see terminal".into());
        }
        match terminal.outcome {
            Outcome::Completed(_) => Ok(0),
            Outcome::Cancelled => Ok(130),
            Outcome::Failed(error) => Err(error.into()),
        }
    }
    .await;
    let shutdown = session.shutdown().await;
    match (result, shutdown) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Ok(code), Ok(())) => Ok(code),
    }
}

/// The history path every action that reads a session carries.
fn action_path(action: &SessionAction) -> Option<PathBuf> {
    match action {
        SessionAction::Info { path }
        | SessionAction::Tree { path }
        | SessionAction::Switch { path }
        | SessionAction::Resources { path }
        | SessionAction::Queue { path }
        | SessionAction::Enqueue { path, .. }
        | SessionAction::QueueMode { path, .. }
        | SessionAction::IncludeAttachment { path, .. }
        | SessionAction::Compact { path, .. }
        | SessionAction::Continue { path }
        | SessionAction::Branch { path, .. }
        | SessionAction::Metadata { path, .. } => Some(path.clone()),
        _ => None,
    }
}
