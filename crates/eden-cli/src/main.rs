use base64::Engine;
use eden_agent::{Outcome, Session, SessionOptions};
use eden_protocol::coding::{Block, LOOP};
use std::path::PathBuf;

fn main() {
    match startup() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
fn startup() -> Result<i32, Box<dyn std::error::Error>> {
    let prepared = eden_cli::environment::prepare(std::env::args().skip(1).collect(), |key| {
        std::env::var_os(key)
    })?;
    for (key, value) in prepared.environment {
        // SAFETY: This synchronous process entry runs before constructing Tokio
        // or loading native plugins; no application threads have been started.
        // The parser validates NUL/equal restrictions before any mutation.
        unsafe {
            std::env::set_var(key, value);
        }
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(prepared.args))
}
async fn run(args: Vec<String>) -> Result<i32, Box<dyn std::error::Error>> {
    if args.first().is_some_and(|arg| {
        matches!(
            arg.as_str(),
            "trust" | "resources" | "commands" | "command" | "package"
        )
    }) {
        return eden_cli::workspace_commands::run(&args).await;
    }
    if args
        .first()
        .is_some_and(|arg| matches!(arg.as_str(), "history" | "session"))
    {
        return eden_cli::session_commands::run(&args).await;
    }
    let mut composition = None;
    let mut json = false;
    let mut prompt = None;
    let mut cwd = std::env::current_dir()?;
    let mut cwd_explicit = false;
    let mut resume = false;
    let mut history = None;
    let mut memory = false;
    let mut attachments = vec![];
    let mut inspect = None;
    let mut workspace_options = eden_agent::WorkspaceOptions::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--composition" => {
                composition = Some(PathBuf::from(
                    args.next().ok_or("--composition needs a path")?,
                ))
            }
            "--cwd" => {
                cwd = PathBuf::from(args.next().ok_or("--cwd needs a directory")?);
                cwd_explicit = true;
            }
            "--continue" => resume = true,
            "--trust-project" => workspace_options.project_trust = Some(true),
            "--no-trust-project" => workspace_options.project_trust = Some(false),
            "--global-dir" => {
                workspace_options.global_dir =
                    PathBuf::from(args.next().ok_or("--global-dir needs a directory")?)
            }
            "--tools" | "--exclude-tools" => {
                let key = if arg == "--tools" {
                    "tools"
                } else {
                    "exclude_tools"
                };
                workspace_options.overrides[key] = serde_json::json!(
                    args.next()
                        .ok_or("tool option needs comma-separated names")?
                        .split(',')
                        .filter(|name| !name.is_empty())
                        .collect::<Vec<_>>()
                );
            }
            "--skill-path" | "--template-path" => {
                let key = if arg == "--skill-path" {
                    "skills"
                } else {
                    "templates"
                };
                if workspace_options.overrides.get(key).is_none() {
                    workspace_options.overrides[key] = serde_json::json!([]);
                }
                workspace_options.overrides[key]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!(
                        args.next().ok_or("resource path is required")?
                    ));
            }
            "--read-only" => workspace_options.overrides["read_only"] = serde_json::json!(true),
            "--no-context" | "--no-skills" | "--no-templates" => {
                let key = match arg.as_str() {
                    "--no-context" => "discover_context",
                    "--no-skills" => "discover_skills",
                    _ => "discover_templates",
                };
                workspace_options.overrides[key] = serde_json::json!(false);
            }
            "--session" | "--resume" => {
                history = Some(PathBuf::from(
                    args.next().ok_or("session option needs a path")?,
                ))
            }
            "--no-session" => memory = true,
            "--history" => {
                inspect = Some(PathBuf::from(args.next().ok_or("--history needs a path")?))
            }
            "--attach" | "--image" | "--file" => {
                attachments.push((arg, args.next().ok_or("attachment option needs a path")?))
            }
            "--json" => json = true,
            "--print" => {}
            "--version" => {
                println!("eden 0.1.0");
                return Ok(0);
            }
            "--help" => {
                println!(
                    "eden trust allow|deny|inspect PATH [--global-dir DIR]\n\
                    eden resources list [--cwd DIR]\n\
                    eden commands\n\
                    eden command NAME JSON\n\
                    eden package install SOURCE [--build]\n\
                    eden session switch PATH --composition COMPOSITION\n\
                    Resource options: --global-dir DIR --trust-project --no-trust-project --no-context --no-skills --no-templates --skill-path PATH --template-path PATH\nTools: --tools NAMES --exclude-tools NAMES --read-only\n\neden [--env-file PATH] [--composition PATH] [--cwd DIR] [--session \
                     PATH|--no-session] [--print|--json] [--attach TEXT|--image IMAGE|--file \
                     PDF] PROMPT\neden --history PATH\neden history inspect|export PATH \
                     [DEST]\neden session info|tree|compact|continue PATH\neden session \
                     fork|clone|import|upgrade|recover|migrate SOURCE DEST [--at NODE] [--cwd \
                     DIR] [--public-only] [--apply]\neden session branch PATH --at NODE --branch \
                     NAME [--summarize]\neden session metadata PATH --name NAME [--tag \
                     TAG]\neden session queue|enqueue|queue-mode|include-attachment PATH \
                     [OPTIONS]\nSession commands accept --composition PATH. Copy commands \
                     preview by default; --apply creates the new file. Explicit model and \
                     credentials are required by the default Responses provider."
                );
                return Ok(0);
            }
            _ if arg.starts_with('-') => return Err(format!("unknown option: {arg}").into()),
            _ if prompt.is_none() => prompt = Some(arg),
            _ => return Err("expected one prompt argument".into()),
        }
    }
    if let Some(path) = inspect {
        for record in eden_kernel::history::read(&path)? {
            println!("{}", serde_json::to_string(&record)?);
        }
        return Ok(0);
    }
    if memory && history.is_some() {
        return Err("--no-session conflicts with --session/--resume".into());
    }
    if let Some(path) = &history
        && path.exists()
        && !cwd_explicit
    {
        let records = eden_kernel::history::read(path)?;
        cwd = PathBuf::from(
            records
                .first()
                .and_then(|r| r.payload["cwd"].as_str())
                .ok_or("missing recorded cwd")?,
        );
    }
    if resume && (prompt.is_some() || history.is_none()) {
        return Err("--continue requires a saved session and no new prompt".into());
    }
    let prompt = if resume {
        String::new()
    } else {
        prompt.ok_or("provide a prompt; see --help")?
    };
    let cwd = std::fs::canonicalize(cwd)?;
    let mut content = vec![Block::Text { text: prompt }];
    for (kind, path) in attachments {
        let path = cwd.join(path);
        let name = path
            .file_name()
            .ok_or("attachment needs a filename")?
            .to_string_lossy()
            .into_owned();
        let data = std::fs::read(&path)?;
        if kind == "--attach" {
            content.push(Block::Text {
                text: format!("Attachment {name}:\n{}", String::from_utf8(data)?),
            });
        } else {
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
            content.push(if kind == "--image" {
                if !media_type.starts_with("image/") {
                    return Err("--image requires an image".into());
                }
                Block::Image { media_type, data }
            } else {
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
            });
        }
    }
    let composition = match composition {
        Some(path) => path,
        None => std::env::current_exe()?
            .parent()
            .and_then(|p| p.parent())
            .ok_or("invalid installation layout")?
            .join("composition.json"),
    };
    let selected: eden_protocol::Composition =
        serde_json::from_slice(&std::fs::read(&composition)?)?;
    if !memory && history.is_none() && selected.roles.contains_key(LOOP) {
        let root = cwd.join(".eden/sessions");
        std::fs::create_dir_all(&root)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        history = Some(root.join(format!("{stamp}-{}.jsonl", std::process::id())));
    }
    if let Some(path) = &history {
        eprintln!("Session: {}", path.display());
    }
    let session = Session::open_with_workspace(
        composition,
        SessionOptions { cwd, history },
        workspace_options,
    )
    .await?;
    let result = async {
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
        let terminal = eden_cli::wait_for_run(&session, run, json).await?;
        if !terminal.cleanup_errors.is_empty() {
            return Err(format!("cleanup failed: {:?}", terminal.cleanup_errors).into());
        }
        match terminal.outcome {
            Outcome::Completed(value) => {
                if !json {
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
