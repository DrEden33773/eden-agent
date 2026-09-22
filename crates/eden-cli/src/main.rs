//! The `eden` process entry point: it installs the explicit environment file, parses, and dispatches.
use base64::Engine;
use eden_agent::{Outcome, Session, SessionOptions};
use eden_cli::cli::{AttachmentKind, Family, Parsed};
use eden_cli::shell::Shell;
use eden_protocol::coding::{Block, LOOP};
use std::ffi::OsString;

fn main() {
    let args: Vec<OsString> = std::env::args_os().collect();
    let startup = match eden_cli::cli::probe(&args) {
        Ok(startup) => startup,
        Err(error) => error.exit(),
    };
    let environment = match &startup.env_file {
        Some(path) => match eden_cli::environment::read(path, |key| std::env::var_os(key)) {
            Ok(environment) => environment,
            Err(error) => eden_cli::cli::fail(error),
        },
        None => vec![],
    };
    for (key, value) in environment {
        // SAFETY: This synchronous process entry runs before constructing Tokio
        // or loading native plugins; no application threads have been started.
        // The parser validates NUL/equal restrictions before any mutation.
        unsafe {
            std::env::set_var(key, value);
        }
    }
    let parsed = eden_cli::cli::parse(&args, startup.color);
    let shell = Shell::new(parsed.color, parsed.cli.quiet, parsed.cli.verbose);
    match start(parsed, &shell) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            shell.error(error);
            std::process::exit(1);
        }
    }
}
fn start(parsed: Parsed, shell: &Shell) -> Result<i32, Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(parsed, shell))
}
async fn run(parsed: Parsed, shell: &Shell) -> Result<i32, Box<dyn std::error::Error>> {
    match &parsed.cli.family {
        Some(Family::Models { .. } | Family::Auth { .. } | Family::Router { .. }) => {
            eden_cli::model_commands::run(&parsed.cli).await
        }
        Some(
            Family::Trust { .. }
            | Family::Resources { .. }
            | Family::Commands
            | Family::Command { .. }
            | Family::Package { .. },
        ) => eden_cli::workspace_commands::run(&parsed.cli, shell).await,
        Some(Family::History { .. } | Family::Session { .. }) => {
            eden_cli::session_commands::run(&parsed.cli, shell).await
        }
        None => prompt(&parsed, shell).await,
    }
}
async fn prompt(parsed: &Parsed, shell: &Shell) -> Result<i32, Box<dyn std::error::Error>> {
    let cli = &parsed.cli;
    if let Some(path) = &cli.history {
        for record in eden_kernel::history::read(path)? {
            println!("{}", serde_json::to_string(&record)?);
        }
        return Ok(0);
    }
    let options = eden_cli::session_options(cli, cli.session.clone(), !cli.continue_session)?;
    let cwd = options.cwd;
    let mut history = options.history;
    let resume = cli.continue_session;
    let prompt = if resume {
        String::new()
    } else {
        cli.prompt.clone().ok_or("provide a prompt; see --help")?
    };
    let cwd = std::fs::canonicalize(cwd)?;
    let mut content = vec![Block::Text { text: prompt }];
    for (kind, path) in eden_cli::cli::attachments(parsed) {
        let path = cwd.join(path);
        let name = path
            .file_name()
            .ok_or("attachment needs a filename")?
            .to_string_lossy()
            .into_owned();
        let data = std::fs::read(&path)?;
        if kind == AttachmentKind::Text {
            content.push(Block::Text {
                text: format!("Attachment {name}:\n{}", String::from_utf8(data)?),
            });
            continue;
        }
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let media_type = match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "pdf" => "application/pdf",
            _ => return Err("unsupported attachment type; use --attach for UTF-8 text".into()),
        }
        .to_owned();
        let data = base64::engine::general_purpose::STANDARD.encode(data);
        content.push(match kind {
            AttachmentKind::Image => {
                if !media_type.starts_with("image/") {
                    return Err("--image requires an image".into());
                }
                Block::Image { media_type, data }
            }
            _ => {
                if media_type != "application/pdf" {
                    return Err(
                        "--file supports PDF; use --image or --attach for other content".into(),
                    );
                }
                Block::File {
                    name,
                    media_type,
                    data,
                }
            }
        });
    }
    let (composition, selected) = eden_cli::load_composition(cli)?;
    if !cli.no_session && history.is_none() && selected.roles.contains_key(LOOP) {
        let root = cwd.join(".eden/sessions");
        std::fs::create_dir_all(&root)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        history = Some(root.join(format!("{stamp}-{}.jsonl", std::process::id())));
    }
    if let Some(path) = &history {
        shell.status("Session:", path.display());
    }
    let session = Session::open_with_workspace(
        composition,
        SessionOptions { cwd, history },
        eden_cli::prompt_options(cli),
    )
    .await?;
    let result = async {
        if let Some(identity) = &cli.model {
            let (provider, model) = identity
                .split_once('/')
                .ok_or("--model requires PROVIDER/MODEL")?;
            let run = session.select_model(eden_protocol::models::ModelSelection {
                provider: provider.into(),
                model: model.into(),
                thinking: cli.thinking.clone(),
            })?;
            session.wait(run).await?.into_result()?;
        }
        let run = if resume {
            session.resume()?
        } else {
            session.submit_blocks(content)?
        };
        let cancel_session = session.clone();
        let signal = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = cancel_session.cancel(run);
            }
        });
        // Aborting this listener only unregisters the CLI listener; run cleanup is awaited below.
        struct Signal(tokio::task::JoinHandle<()>);
        impl Drop for Signal {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _signal = Signal(signal);
        let terminal = eden_cli::wait_for_run(&session, run, cli.json, shell).await?;
        if !terminal.cleanup_errors.is_empty() {
            return Err(format!("cleanup failed: {:?}", terminal.cleanup_errors).into());
        }
        match terminal.outcome {
            Outcome::Completed(value) => {
                if !cli.json {
                    use std::io::Write;
                    writeln!(std::io::stdout().lock(), "{}", value.as_str().unwrap_or(""))?;
                }
                Ok(0)
            }
            Outcome::Cancelled => Ok(130),
            Outcome::Failed(error) => Err(Box::new(error) as Box<dyn std::error::Error>),
        }
    }
    .await;
    let shutdown = session.shutdown().await;
    match (result, shutdown) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(Box::new(error)),
        (Ok(code), Ok(())) => Ok(code),
    }
}
