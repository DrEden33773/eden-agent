//! Scriptable model selection and transient login interaction through the same SDK contracts.
use crate::cli::{AuthAction, Cli, Family, ModelAction};
use eden_agent::{Session, SessionOptions};
use eden_protocol::models::{AuthRequest, CatalogRequest, ModelSelection};
use serde_json::{Value, json};
use std::{
    error::Error,
    future::Future,
    io::{BufRead, Read, Write},
    pin::Pin,
    process::Stdio,
};
use tokio::io::AsyncReadExt;

async fn cancellable(session: &Session, run: u64) -> Result<Value, Box<dyn Error>> {
    let signal = tokio::signal::ctrl_c();
    tokio::pin!(signal);
    cancellable_with_signal(session, run, signal.as_mut()).await
}

async fn cancellable_with_signal(
    session: &Session,
    run: u64,
    signal: Pin<&mut impl Future<Output = std::io::Result<()>>>,
) -> Result<Value, Box<dyn Error>> {
    tokio::select! {
        biased;
        signal = signal => {
            session.cancel(run)?;
            let _ = session.wait(run).await?;
            signal?;
            Err("authentication cancelled".into())
        },
        result = settled(session, run) => result,
    }
}

async fn login(
    session: &Session,
    provider: &str,
    method: Option<String>,
) -> Result<Value, Box<dyn Error>> {
    // Retain the same registered receiver across Login, UI handoff and Wait.
    // Recreating ctrl_c after printing the prompt can lose a signal in that gap.
    let signal = tokio::signal::ctrl_c();
    tokio::pin!(signal);
    let start = cancellable_with_signal(
        session,
        session.authenticate(AuthRequest::Login {
            provider: provider.into(),
            method,
        })?,
        signal.as_mut(),
    )
    .await?;
    let reply: eden_protocol::models::AuthReply = serde_json::from_value(start.clone())?;
    let Some(interaction) = reply.interaction else {
        return Ok(start);
    };
    let operation_id = reply.operation_id.ok_or("missing auth operation")?;
    eprintln!("Open: {}", interaction.url);
    if let Some(code) = interaction.user_code {
        eprintln!("Code: {code}");
    }
    // A killable child owns the blocking terminal read. Tokio's stdin uses a blocking
    // task that cannot be aborted and can otherwise prevent runtime shutdown.
    let mut reader = if interaction.manual_input {
        eprintln!(
            "Paste the authorization code or redirect URL, then press Enter (or finish in the \
             browser)."
        );
        Some(
            tokio::process::Command::new(std::env::current_exe()?)
                .args(["auth", "read-input"])
                .stdin(Stdio::inherit())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()?,
        )
    } else {
        None
    };
    let run = session.authenticate(AuthRequest::Wait {
        operation_id: operation_id.clone(),
    })?;
    let result = async {
        if let Some(child) = reader.as_mut() {
            let mut stdout = child.stdout.take().ok_or("missing private input pipe")?;
            let input = async {
                let mut bytes = Vec::new();
                stdout.read_to_end(&mut bytes).await?;
                if !child.wait().await?.success() {
                    return Err::<(), Box<dyn Error>>("could not read authorization input".into());
                }
                let input =
                    String::from_utf8(bytes).map_err(|_| "authorization input must be UTF-8")?;
                if !input.trim().is_empty() {
                    session
                        .submit_auth_input(&operation_id, input.trim().into())
                        .await?;
                }
                Ok(())
            };
            tokio::select! {
                result = cancellable_with_signal(session, run, signal.as_mut()) => return result,
                result = input => result?,
            }
        }
        cancellable_with_signal(session, run, signal.as_mut()).await
    }
    .await;
    if result.is_err() {
        session.cancel(run)?;
        let _ = session.wait(run).await?;
    }
    if let Some(mut child) = reader {
        // Always reap before reporting completion, including callback wins and Ctrl-C.
        child.kill().await?;
        child.wait().await?;
    }
    result
}

async fn settled(session: &Session, run: u64) -> Result<Value, Box<dyn Error>> {
    Ok(session.wait(run).await?.into_result()?)
}
/// Commands always close the session, including when an operation fails.
pub async fn run(cli: &Cli) -> Result<i32, Box<dyn Error>> {
    if matches!(
        cli.family,
        Some(Family::Auth {
            action: AuthAction::ReadInput
        })
    ) {
        let mut input = String::new();
        std::io::stdin().lock().take(65537).read_line(&mut input)?;
        if input.len() > 65536 {
            return Err("authorization input too large".into());
        }
        std::io::stdout().write_all(input.as_bytes())?;
        return Ok(0);
    }
    let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
    let session = Session::open_with_workspace(
        crate::composition(cli)?,
        SessionOptions {
            cwd,
            history: cli.session.clone(),
        },
        crate::workspace_options(cli),
    )
    .await?;
    let result = async {
        let value =
            match cli.family.as_ref().ok_or("missing operation")? {
                Family::Models { action } => match action {
                    ModelAction::List => json!(session.models().await?),
                    ModelAction::Current => json!(session.model_selection().await?),
                    ModelAction::Refresh => {
                        settled(&session, session.catalog(CatalogRequest::Refresh)?).await?
                    }
                    ModelAction::Source { url } => {
                        settled(
                            &session,
                            session.catalog(CatalogRequest::SetSource { url: url.clone() })?,
                        )
                        .await?
                    }
                    ModelAction::Select {
                        provider,
                        model,
                        thinking,
                    } => {
                        if cli.session.is_none() {
                            return Err("models select requires --session; use models default \
                                        for a global default"
                                .into());
                        }
                        settled(
                            &session,
                            session.select_model(ModelSelection {
                                provider: provider.clone(),
                                model: model.clone(),
                                thinking: thinking.clone(),
                            })?,
                        )
                        .await?
                    }
                    ModelAction::Default {
                        provider,
                        model,
                        thinking,
                    } => {
                        settled(
                            &session,
                            session.catalog(CatalogRequest::SetDefault {
                                selection: ModelSelection {
                                    provider: provider.clone(),
                                    model: model.clone(),
                                    thinking: thinking.clone(),
                                },
                            })?,
                        )
                        .await?
                    }
                    ModelAction::Cycle => {
                        if cli.session.is_none() {
                            return Err("models cycle requires --session".into());
                        }
                        let catalog = session.models().await?;
                        let models: Vec<_> = catalog
                            .models
                            .into_iter()
                            .filter(|entry| entry.status == "configured")
                            .collect();
                        if models.is_empty() {
                            return Err("no configured models available".into());
                        }
                        let current = session.model_selection().await?;
                        let index = current
                            .and_then(|current| {
                                models.iter().position(|entry| {
                                    entry.target.provider == current.provider
                                        && entry.target.model == current.model
                                })
                            })
                            .map_or(0, |index| (index + 1) % models.len());
                        let target = &models[index].target;
                        settled(
                            &session,
                            session.select_model(ModelSelection {
                                provider: target.provider.clone(),
                                model: target.model.clone(),
                                thinking: target.thinking.requested.clone(),
                            })?,
                        )
                        .await?
                    }
                },
                Family::Auth { action } => match action {
                    AuthAction::Login { provider, method } => {
                        login(&session, provider, method.clone()).await?
                    }
                    AuthAction::Refresh { provider } => {
                        cancellable(
                            &session,
                            session.authenticate(AuthRequest::Refresh {
                                provider: provider.clone(),
                            })?,
                        )
                        .await?
                    }
                    AuthAction::ReadInput => {
                        unreachable!("private input helper returned before session creation")
                    }
                    AuthAction::Logout { provider } => {
                        settled(
                            &session,
                            session.authenticate(AuthRequest::Logout {
                                provider: provider.clone(),
                            })?,
                        )
                        .await?
                    }
                    AuthAction::Set { provider } => {
                        let start = settled(
                            &session,
                            session.authenticate(AuthRequest::Start {
                                provider: provider.clone(),
                            })?,
                        )
                        .await?;
                        let operation_id = start["operation_id"]
                            .as_str()
                            .ok_or("missing auth operation")?
                            .to_owned();
                        let mut api_key = String::new();
                        std::io::stdin().take(65537).read_to_string(&mut api_key)?;
                        if api_key.len() > 65536 {
                            return Err("API key input too large".into());
                        }
                        settled(
                            &session,
                            session.authenticate(AuthRequest::Input {
                                operation_id,
                                api_key: api_key.trim().into(),
                            })?,
                        )
                        .await?
                    }
                },
                _ => return Err("not a model operation".into()),
            };
        println!("{}", serde_json::to_string(&value)?);
        Ok(0)
    }
    .await;
    let shutdown = session.shutdown().await;
    match (result, shutdown) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Ok(code), Ok(())) => Ok(code),
    }
}
