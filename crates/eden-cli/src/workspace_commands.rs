//! Text resources, explicit trust and contributed commands through the shared SDK.
use crate::cli::{Cli, Family, PackageAction, TrustAction};
use crate::shell::Shell;
use eden_agent::{Session, SessionOptions};
use serde_json::{Value, json};
use std::{error::Error, path::PathBuf};
type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Run the `resources`, `trust`, `commands` and `package` families.
pub async fn run(cli: &Cli, shell: &Shell) -> Result<i32> {
    let family = cli.family.as_ref().ok_or("workspace command required")?;
    if let Family::Trust { action } = family {
        let options = crate::workspace_options(cli);
        let directory = match &cli.cwd {
            Some(directory) => directory.clone(),
            None => std::env::current_dir()?,
        };
        let path = directory.join(trust_path(action));
        match action {
            TrustAction::Allow { .. } | TrustAction::Deny { .. } => {
                let trusted = matches!(action, TrustAction::Allow { .. });
                eden_agent::save_trust(&options.global_dir, &path, trusted)?;
                println!(
                    "{}",
                    json!({ "path": std::fs::canonicalize(path)?, "trusted": trusted })
                );
            }
            TrustAction::Inspect { .. } => println!(
                "{}",
                serde_json::to_string(&eden_agent::Workspace::discover(&path, &options)?)?
            ),
        }
        return Ok(0);
    }
    let cwd = match &cli.cwd {
        Some(directory) => directory.clone(),
        None => std::env::current_dir()?,
    };
    let session = Session::open_with_workspace(
        crate::composition(cli)?,
        SessionOptions { cwd, history: None },
        crate::workspace_options(cli),
    )
    .await?;
    for event in session
        .events()
        .iter()
        .filter(|event| event.kind == "resource_diagnostic")
    {
        shell.diagnostic(&crate::resource_diagnostic(&event.payload));
    }
    let result = async {
        match family {
            Family::Resources { .. } => {
                println!("{}", serde_json::to_string(&session.resources().await?)?);
                Ok(0)
            }
            Family::Commands => {
                println!("{}", serde_json::to_string(&session.commands().await?)?);
                Ok(0)
            }
            Family::Package { action } => {
                let (name, arguments) = package_action(action)?;
                invoke(&session, name, arguments).await
            }
            Family::Command { name, arguments } => {
                invoke(&session, name.clone(), serde_json::from_str(arguments)?).await
            }
            _ => Err("workspace command required".into()),
        }
    }
    .await;
    let stopped = session.shutdown().await;
    match (result, stopped) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Ok(code), Ok(())) => Ok(code),
    }
}

/// The path a trust action names; an omitted path means the working directory.
fn trust_path(action: &TrustAction) -> &PathBuf {
    match action {
        TrustAction::Allow { path }
        | TrustAction::Deny { path }
        | TrustAction::Inspect { path } => path,
    }
}

/// The shared command name and JSON arguments one package action produces.
fn package_action(action: &PackageAction) -> Result<(String, Value)> {
    Ok(match action {
        PackageAction::Install {
            source,
            source_json,
            build,
        } => {
            let source = match source_json {
                Some(source) => serde_json::from_str(source)?,
                None => json!({
                    "kind": "local",
                    "path": source
                        .clone()
                        .ok_or("install requires SOURCE or --source-json")?,
                }),
            };
            (
                "package.install".to_owned(),
                json!({ "source": source, "build": build }),
            )
        }
        PackageAction::List => ("package.list".to_owned(), json!({})),
        PackageAction::Remove {
            name,
            version,
            force,
        } => (
            "package.remove".to_owned(),
            json!({ "name": name, "version": version, "force": force }),
        ),
        PackageAction::Resolve { arguments } => (
            "package.resolve".to_owned(),
            serde_json::from_str(arguments)?,
        ),
    })
}

/// One contributed command or package action, with cancellation wiring.
async fn invoke(session: &Session, name: String, arguments: Value) -> Result<i32> {
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
    Ok(0)
}
