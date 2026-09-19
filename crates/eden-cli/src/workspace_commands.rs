//! Text resources, explicit trust and contributed commands through the shared SDK.
use eden_agent::{Session, SessionOptions, WorkspaceOptions};
use serde_json::{Value, json};
use std::{error::Error, path::PathBuf};
pub async fn run(args: &[String]) -> Result<i32, Box<dyn Error>> {
    let family = args[0].as_str();
    let mut options = WorkspaceOptions::default();
    let mut cwd = std::env::current_dir()?;
    let mut composition = None;
    let mut positional = vec![];
    let mut source = None;
    let mut build = false;
    let mut force = false;
    let mut iter = args[1..].iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--cwd" => cwd = PathBuf::from(iter.next().ok_or("missing cwd")?),
            "--global-dir" => {
                options.global_dir = PathBuf::from(iter.next().ok_or("missing global directory")?)
            }
            "--composition" => {
                composition = Some(PathBuf::from(iter.next().ok_or("missing composition")?))
            }
            "--trust-project" => options.project_trust = Some(true),
            "--no-trust-project" => options.project_trust = Some(false),
            "--source-json" => {
                source = Some(serde_json::from_str::<Value>(
                    iter.next().ok_or("missing source JSON")?,
                )?)
            }
            "--build" => build = true,
            "--force" => force = true,
            _ if arg.starts_with('-') => return Err(format!("unknown option: {arg}").into()),
            _ => positional.push(arg.as_str()),
        }
    }
    if family == "trust" {
        let action = positional
            .first()
            .copied()
            .ok_or("trust requires allow, deny or inspect")?;
        let path = cwd.join(positional.get(1).copied().unwrap_or("."));
        match action {
            "allow" | "deny" => {
                eden_agent::save_trust(&options.global_dir, &path, action == "allow")?;
                println!(
                    "{}",
                    json!({ "path": std::fs::canonicalize(path)?, "trusted": action == "allow" })
                );
            }
            "inspect" => println!(
                "{}",
                serde_json::to_string(&eden_agent::Workspace::discover(&path, &options)?)?
            ),
            _ => return Err("unknown trust action".into()),
        }
        return Ok(0);
    }
    let composition = composition.unwrap_or(
        std::env::current_exe()?
            .parent()
            .ok_or("executable has no parent")?
            .join("../composition.json"),
    );
    let session =
        Session::open_with_workspace(composition, SessionOptions { cwd, history: None }, options)
            .await?;
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
    let result = async {
        if family == "resources" {
            if positional.first().is_some_and(|name| *name != "list") {
                return Err(
                    "resources supports list; use Session.reload_resources for a live session"
                        .into(),
                );
            }
            println!("{}", serde_json::to_string(&session.resources().await?)?);
            return Ok(0);
        }
        if family == "commands" {
            println!("{}", serde_json::to_string(&session.commands().await?)?);
            return Ok(0);
        }
        let (name, arguments) = if family == "package" {
            let action = positional
                .first()
                .copied()
                .ok_or("package requires install, list, remove or resolve")?;
            let arguments = match action {
                "install" => json!({
                    "source": match source {
                        Some(source) => source,
                        None => json!({
                            "kind": "local",
                            "path": positional
                                .get(1)
                                .ok_or("install requires SOURCE or --source-json")?,
                        }),
                    },
                    "build": build,
                }),
                "list" => json!({}),
                "remove" => json!({
                    "name": positional.get(1).ok_or("missing name")?,
                    "version": positional.get(2).ok_or("missing version")?,
                    "force": force,
                }),
                "resolve" => serde_json::from_str(
                    positional.get(1).ok_or("resolve requires JSON arguments")?,
                )?,
                _ => return Err("unknown package command".into()),
            };
            (format!("package.{action}"), arguments)
        } else {
            (
                positional
                    .first()
                    .ok_or("missing command name")?
                    .to_string(),
                serde_json::from_str(positional.get(1).copied().unwrap_or("{}"))?,
            )
        };
        let run = session.command(name, arguments)?;
        let cancelling = session.clone();
        let signal = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = cancelling.cancel(run);
            }
        });
        let result = session.wait(run).await;
        signal.abort();
        let value = result?.into_result()?;
        println!("{}", serde_json::to_string(&value)?);
        Ok::<_, Box<dyn Error>>(0)
    }
    .await;
    let stopped = session.shutdown().await;
    match (result, stopped) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Ok(code), Ok(())) => Ok(code),
    }
}
