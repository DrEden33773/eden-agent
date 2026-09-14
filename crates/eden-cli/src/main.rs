use base64::Engine;
use eden_agent::{Outcome, Session, SessionOptions};
use eden_protocol::coding::{Block, LOOP};
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    match run().await {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
async fn run() -> Result<i32, Box<dyn std::error::Error>> {
    let mut composition = None;
    let mut json = false;
    let mut prompt = None;
    let mut cwd = std::env::current_dir()?;
    let mut history = None;
    let mut memory = false;
    let mut attachments = vec![];
    let mut inspect = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--composition" => {
                composition = Some(PathBuf::from(
                    args.next().ok_or("--composition needs a path")?,
                ))
            }
            "--cwd" => cwd = PathBuf::from(args.next().ok_or("--cwd needs a directory")?),
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
                    "eden [--composition PATH] [--cwd DIR] [--session PATH|--no-session] [--print|--json] [--attach TEXT|--image IMAGE|--file PDF] PROMPT\neden --history PATH\nExplicit model and credentials are required by the default Responses provider."
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
    let prompt = prompt.ok_or("provide a prompt; see --help")?;
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
    let session = Session::open_with(composition, SessionOptions { cwd, history }).await?;
    let result = async {
        let run = session.submit_blocks(content)?;
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
        let terminal = if json {
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
                    break session.wait(run).await?;
                }
            }
        } else {
            session.wait(run).await?
        };
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
