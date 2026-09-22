//! Command-specific use of root arguments, checked before runtime or file access.
use crate::cli::{Cli, Family, ModelAction};
use clap::{ArgMatches, parser::ValueSource};

/// Reject explicit options that the selected operation would otherwise discard.
pub(crate) fn validate(cli: &Cli, matches: &ArgMatches) -> Result<(), String> {
    let prompt_options = [
        "model",
        "thinking",
        "continue_session",
        "no_session",
        "history",
        "tools",
        "exclude_tools",
        "skill_path",
        "template_path",
        "read_only",
        "no_context",
        "no_skills",
        "no_templates",
        "attach",
        "image",
        "file",
    ];
    let explicit = |id: &str| matches.value_source(id) == Some(ValueSource::CommandLine);
    let flag = |id: &str| match id {
        "continue_session" => "--continue".to_owned(),
        _ => format!("--{}", id.replace('_', "-")),
    };
    if let Some(family) = &cli.family {
        if matches!(family, Family::Rpc) && cli.print {
            return Err("--print cannot be combined with rpc".into());
        }
        for id in prompt_options {
            if explicit(id) {
                return Err(format!(
                    "{} is only valid for a prompt run, not this command",
                    flag(id)
                ));
            }
        }
        if cli.session.is_some()
            && !matches!(
                family,
                Family::Models { .. } | Family::Auth { .. } | Family::Router { .. } | Family::Rpc
            )
        {
            return Err(
                "--session/--resume is not accepted by this command; use its history path argument"
                    .into(),
            );
        }
        if matches!(
            family,
            Family::Models {
                action: ModelAction::Select { .. } | ModelAction::Cycle
            }
        ) && cli.session.is_none()
        {
            return Err(
                "this model operation requires --session PATH; use models default for a global \
                 default"
                    .into(),
            );
        }
    } else if cli.history.is_some() {
        if cli.session.is_some() || !cli.prompt.is_empty() {
            return Err("--history cannot be combined with --session/--resume or a prompt".into());
        }
        for id in prompt_options.into_iter().filter(|id| *id != "history") {
            if explicit(id) {
                return Err(format!("{} cannot be combined with --history", flag(id)));
            }
        }
    } else {
        if let Some(model) = &cli.model
            && !model
                .split_once('/')
                .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
        {
            return Err("--model requires nonempty PROVIDER/MODEL".into());
        }
        if cli.continue_session
            && (!cli.attach.is_empty() || !cli.image.is_empty() || !cli.file.is_empty())
        {
            return Err(
                "--continue does not accept --attach, --image or --file; submit a new prompt to \
                 add content"
                    .into(),
            );
        }
    }
    Ok(())
}
