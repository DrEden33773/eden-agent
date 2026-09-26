//! Ordinary configuration edits share SDK validation, transaction ownership and receipts.
use crate::cli::{Cli, ConfigAction, ConfigChangeArgs, Family};
use eden_agent::{
    Session,
    configuration::{ApplyMode, Change, Status},
};
use serde_json::{Value, json};
use std::error::Error;

fn change(args: &ConfigChangeArgs) -> Result<Change, Box<dyn Error>> {
    let patch = serde_json::from_str(&args.patch).map_err(|_| "--patch must be valid JSON")?;
    Ok(Change {
        instance: args.instance.clone(),
        revision: args.revision,
        patch,
        replacement: args.replacement.clone(),
    })
}

/// Keep failure receipts on stdout while returning a failure exit code for scripts.
pub async fn run(cli: &Cli) -> Result<i32, Box<dyn Error>> {
    let Some(Family::Config { action }) = &cli.family else {
        return Err("not a configuration operation".into());
    };
    // Parse before opening native libraries so a malformed candidate cannot cause startup effects.
    let candidate = match action {
        ConfigAction::Validate { change: args }
        | ConfigAction::Preview { change: args }
        | ConfigAction::Apply { change: args, .. } => Some(change(args)?),
        _ => None,
    };
    let options = crate::session_options(cli, cli.session.clone(), false)?;
    let session = Session::open_with_workspace(
        crate::composition(cli)?,
        options,
        crate::workspace_options(cli),
    )
    .await?;
    let result = async {
        let (value, code): (Value, i32) = match action {
            ConfigAction::Inspect => (json!(session.inspect_configuration().await?), 0),
            ConfigAction::Validate { .. } => {
                let result = session
                    .validate_configuration(candidate.ok_or("missing configuration change")?)
                    .await?;
                let code = i32::from(!result.errors.is_empty());
                (json!(result), code)
            }
            ConfigAction::Preview { .. } => (
                json!(
                    session
                        .preview_configuration(candidate.ok_or("missing configuration change")?)
                        .await?
                ),
                0,
            ),
            ConfigAction::Apply { mode, .. } => {
                let mode = match mode.as_str() {
                    "wait" => ApplyMode::Wait,
                    "cancel" => ApplyMode::Cancel,
                    _ => return Err("invalid application mode".into()),
                };
                let operation = session
                    .apply_configuration(candidate.ok_or("missing configuration change")?, mode)
                    .await?;
                let receipt = session.wait_configuration(operation).await?;
                let code = i32::from(receipt.status != Status::Applied);
                (json!(receipt), code)
            }
            ConfigAction::Status { operation } => {
                (json!(session.configuration_operation(*operation)?), 0)
            }
        };
        crate::output::result(cli, &value)?;
        Ok(code)
    }
    .await;
    let shutdown = session.shutdown().await;
    match (result, shutdown) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Ok(code), Ok(())) => Ok(code),
    }
}
