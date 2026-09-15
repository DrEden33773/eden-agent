//! Noninteractive history and session commands using the shared Session API.
use eden_agent::{CopyKind, CopyOptions, Outcome, Session};
use eden_protocol::coding::Block;
use serde_json::json;
use std::{error::Error, io::Write, path::PathBuf};
type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub async fn run(args: &[String]) -> Result<i32> {
    let family = args.first().ok_or("missing command")?.as_str();
    let command = args
        .get(1)
        .ok_or("expected history inspect/export or session command")?
        .as_str();
    let mut positional = vec![];
    let mut composition = None;
    let mut workspace = eden_agent::WorkspaceOptions::default();
    let mut cwd = None;
    let mut target = None;
    let mut branch = None;
    let mut instructions = String::new();
    let mut name = String::new();
    let mut tags = vec![];
    let mut json_output = false;
    let mut apply = false;
    let mut public_only = false;
    let mut summarize = false;
    let mut steering = "one".to_owned();
    let mut follow_up = "one".to_owned();
    let mut kind = "follow_up".to_owned();
    let mut iter = args[2..].iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--composition" => {
                composition = Some(PathBuf::from(
                    iter.next().ok_or("missing composition path")?,
                ))
            }
            "--global-dir" => {
                workspace.global_dir = PathBuf::from(iter.next().ok_or("missing global directory")?)
            }
            "--trust-project" => workspace.project_trust = Some(true),
            "--no-trust-project" => workspace.project_trust = Some(false),
            "--cwd" => cwd = Some(PathBuf::from(iter.next().ok_or("missing cwd")?)),
            "--at" => target = Some(iter.next().ok_or("missing node id")?.parse::<u64>()?),
            "--branch" => branch = Some(iter.next().ok_or("missing branch")?.clone()),
            "--instructions" => instructions = iter.next().ok_or("missing instructions")?.clone(),
            "--name" => name = iter.next().ok_or("missing name")?.clone(),
            "--tag" => tags.push(iter.next().ok_or("missing tag")?.clone()),
            "--steering" => steering = iter.next().ok_or("missing steering mode")?.clone(),
            "--follow-up" => follow_up = iter.next().ok_or("missing follow-up mode")?.clone(),
            "--kind" => kind = iter.next().ok_or("missing queue kind")?.clone(),
            "--apply" => apply = true,
            "--public-only" => public_only = true,
            "--summarize" => summarize = true,
            "--json" => json_output = true,
            _ if arg.starts_with('-') => {
                return Err(format!("unknown session option: {arg}").into());
            }
            _ => positional.push(arg.clone()),
        }
    }
    let path = PathBuf::from(positional.first().ok_or("missing history path")?);
    if family == "history" || matches!(command, "info" | "tree") {
        let scan = eden_kernel::history::inspect(&path)?;
        if command == "export" {
            if let Some(error) = scan.diagnostic {
                return Err(error.into());
            }
            let destination = PathBuf::from(positional.get(1).ok_or("missing export destination")?);
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
            return Ok(0);
        }
        if command == "info" {
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
                    "session_id": scan.records.first().map(|r|r.session_id),
                    "records": scan.records.len(),
                    "head": state.0,
                    "branch": state.1,
                    "binding": scan.records.first().map(|r|&r.payload),
                    "metadata": metadata,
                    "diagnostic": scan.diagnostic
                })
            )?;
        } else if command == "tree" {
            for record in &scan.records {
                writeln!(
                    std::io::stdout(),
                    "{}",
                    json!({
                        "id": record.sequence,
                        "parent": record.parent_id,
                        "branch": record.branch,
                        "kind": record.kind
                    })
                )?;
            }
        } else if command == "inspect" {
            for record in &scan.records {
                writeln!(std::io::stdout(), "{}", serde_json::to_string(record)?)?;
            }
        } else {
            return Err("unknown history command".into());
        }
        if let Some(error) = scan.diagnostic {
            writeln!(std::io::stderr(), "{error}")?;
            return Ok(2);
        }
        return Ok(0);
    }
    let composition = composition.unwrap_or(
        std::env::current_exe()?
            .parent()
            .and_then(|p| p.parent())
            .ok_or("invalid installation")?
            .join("composition.json"),
    );
    let copy_kind = match command {
        "fork" => Some(CopyKind::Fork),
        "clone" => Some(CopyKind::Clone),
        "import" => Some(CopyKind::Import),
        "upgrade" => Some(CopyKind::Upgrade),
        "recover" => Some(CopyKind::Recover),
        "migrate" => Some(CopyKind::Migrate),
        _ => None,
    };
    if let Some(copy_kind) = copy_kind {
        let destination = PathBuf::from(positional.get(1).ok_or("missing destination")?);
        let plan = Session::plan_copy(
            &composition,
            CopyOptions {
                source: path,
                destination,
                kind: copy_kind,
                target,
                cwd,
                public_only,
            },
        )
        .await?;
        writeln!(std::io::stdout(), "{}", serde_json::to_string(&plan)?)?;
        if apply {
            writeln!(
                std::io::stdout(),
                "{}",
                json!({"created":Session::apply_copy(plan).await?})
            )?;
        }
        return Ok(0);
    }
    let cwd = match cwd {
        Some(cwd) => cwd,
        None => PathBuf::from(
            eden_kernel::history::read(&path)?
                .first()
                .and_then(|r| r.payload["cwd"].as_str())
                .ok_or("missing recorded cwd")?,
        ),
    };
    let options = eden_agent::SessionOptions {
        cwd,
        history: Some(path),
    };
    let session = if command == "switch" {
        Session::open_rebound(&composition, options, workspace).await?
    } else {
        Session::open_with_workspace(&composition, options, workspace).await?
    };
    let result = async {
        if command == "switch" {
            writeln!(
                std::io::stdout(),
                "{{\"available\":true,\"binding_updated\":true}}"
            )?;
            return Ok(0);
        }
        if command == "resources" {
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string(&session.resources().await?)?
            )?;
            return Ok(0);
        }
        if command == "queue" {
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string(&session.queued().await?)?
            )?;
            return Ok(0);
        }
        if command == "enqueue" {
            let text = positional.get(1).ok_or("enqueue requires text")?.clone();
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string(&session.enqueue(&kind, vec![Block::Text { text }]).await?)?
            )?;
            return Ok(0);
        }
        let run = match command {
            "branch" => session.navigate(
                target.ok_or("branch requires --at NODE")?,
                branch.ok_or("branch requires --branch NAME")?,
                summarize,
            )?,
            "compact" => session.compact(instructions)?,
            "metadata" => session.set_metadata(name, tags)?,
            "queue-mode" => session.configure_queue(steering, follow_up)?,
            "include-attachment" => session
                .include_attachment(target.ok_or("include-attachment requires --at NODE")?)?,
            "continue" => session.resume()?,
            _ => return Err("unknown session command".into()),
        };
        let cancel_session = session.clone();
        let signal = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = cancel_session.cancel(run);
            }
        });
        let terminal = crate::wait_for_run(&session, run, json_output).await;
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
