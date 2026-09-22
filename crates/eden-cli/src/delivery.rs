//! Delivery commands own only the selected independent services and always close them.
use crate::cli::{Cli, Family};
use eden_agent::delivery::{Delivery, read_preview, save_preview};
use eden_plugin_sdk::Cancellation;
use eden_protocol::{delivery::*, updates::UPDATE_SOURCE};
use serde_json::json;
use std::error::Error;
/// Execute independently of the conversation's original plugin composition.
pub async fn run(cli: &Cli) -> Result<i32, Box<dyn Error>> {
    if matches!(cli.family, Some(Family::InstallationCheck)) {
        return installation_check(cli).await;
    }
    let role = match &cli.family {
        Some(Family::Export { .. }) => EXPORTER,
        Some(Family::Share { .. }) => SHARE_TARGET,
        Some(Family::Update { .. }) => UPDATE_SOURCE,
        _ => return Err("delivery command required".into()),
    };
    let delivery = Delivery::open_with_global(
        crate::composition(cli)?,
        &[role],
        &crate::workspace_options(cli).global_dir,
    )
    .await?;
    let cancel = Cancellation::default();
    let signal_cancel = cancel.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });
    let result = async {
        match &cli.family {
            Some(Family::Export {
                path,
                destination,
                selection,
                jsonl,
            }) => {
                let artifact = delivery
                    .export_file(
                        path,
                        serde_json::from_str(selection)?,
                        if *jsonl { Format::Jsonl } else { Format::Html },
                    )
                    .await?;
                let digest = save_preview(&artifact, destination)?;
                crate::output::result(
                    cli,
                    &json!({
                        "preview": destination,
                        "sha256": digest,
                        "warnings": artifact.warnings,
                        "notice":
                            "Review this file before explicitly publishing it. Ordinary text and \
                             tool output may contain sensitive information.",
                    }),
                )?;
            }
            Some(Family::Share {
                path,
                sha256,
                confirm,
            }) => {
                if !confirm {
                    return Err("review the file and pass --confirm to authorize remote \
                                publication"
                        .into());
                }
                let artifact = read_preview(path, sha256)?;
                let reply = delivery
                    .invoke(
                        SHARE_TARGET,
                        json!(PublishRequest {
                            artifact,
                            confirmed: true
                        }),
                        cancel,
                    )
                    .await?;
                crate::output::result(cli, &reply)?;
            }
            Some(Family::Update { request }) => {
                let request: eden_protocol::updates::UpdateRequest = serde_json::from_str(request)?;
                crate::output::result(
                    cli,
                    &delivery
                        .invoke(UPDATE_SOURCE, json!(request), cancel)
                        .await?,
                )?;
            }
            _ => return Err("delivery command required".into()),
        }
        Ok(0)
    }
    .await;
    signal.abort();
    let stopped = delivery.shutdown().await;
    match (result, stopped) {
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(e.into()),
        (Ok(code), Ok(())) => Ok(code),
    }
}
async fn installation_check(cli: &Cli) -> Result<i32, Box<dyn Error>> {
    let composition = crate::composition(cli)?;
    let root = composition
        .parent()
        .ok_or("installation composition has no parent")?;
    let mut selected: eden_protocol::Composition =
        serde_json::from_slice(&std::fs::read(&composition)?)?;
    if selected
        .packages
        .iter()
        .any(|p| p.descriptor.package == "search")
        && !root
            .join("bin")
            .join(if cfg!(windows) {
                "eden-search-worker.exe"
            } else {
                "eden-search-worker"
            })
            .is_file()
    {
        return Err("installation search worker missing".into());
    }
    let scratch =
        std::env::temp_dir().join(format!("eden-installation-check-{}", std::process::id()));
    for package in &mut selected.packages {
        if !package.config.is_object() {
            package.config = json!({});
        }
        match package.descriptor.package.as_str() {
            "distribution" => package.config["root"] = json!(scratch.join("distribution")),
            "workspace-resources" => {
                package.config = json!({
                    "cwd": root,
                    "global_dir": scratch,
                    "trusted": false,
                    "settings": {
                        "discover_context": false,
                        "discover_skills": false,
                        "discover_templates": false,
                    },
                })
            }
            "model-access" => {
                package.config["global_dir"] = json!(scratch);
                package.config["offline"] = json!(true);
            }
            _ => {}
        }
    }
    let kernel =
        eden_kernel::Kernel::load_resolved(selected, root, 0, eden_kernel::Events::new(0)).await?;
    kernel.shutdown().await?;
    crate::output::result(cli, &json!({ "installation": "ready", "path": root }))?;
    Ok(0)
}
