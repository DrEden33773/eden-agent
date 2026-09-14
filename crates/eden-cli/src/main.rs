use eden_agent::{Outcome, Session};
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
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--composition" => {
                composition = Some(PathBuf::from(
                    args.next().ok_or("--composition needs a path")?,
                ))
            }
            "--json" => json = true,
            "--print" => {}
            "--version" => {
                println!("eden 0.1.0");
                return Ok(0);
            }
            "--help" => {
                println!(
                    "eden [--composition PATH] [--print|--json] PROMPT\nControlled native-plugin development release."
                );
                return Ok(0);
            }
            _ if arg.starts_with('-') => return Err(format!("unknown option: {arg}").into()),
            _ if prompt.is_none() => prompt = Some(arg),
            _ => return Err("expected one prompt argument".into()),
        }
    }
    let prompt = prompt.ok_or("provide a prompt; see --help")?;
    let composition = match composition {
        Some(path) => path,
        None => std::env::current_exe()?
            .parent()
            .and_then(|p| p.parent())
            .ok_or("invalid installation layout")?
            .join("composition.json"),
    };
    let session = Session::open(composition).await?;
    let result = async {
        let run = session.submit(prompt)?;
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
