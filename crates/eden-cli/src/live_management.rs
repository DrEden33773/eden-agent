//! Launching another local host is an explicit management action; accepted work stays with its owner.
use super::*;
use eden_agent::{CopyOptions, CopyPlan};

#[derive(Default)]
pub(super) struct Management {
    pub(super) launch: Option<Cli>,
    pub(super) copies: Mutex<HashMap<String, CopyPlan>>,
}
pub(super) async fn management_read(
    shared: &Shared,
    route: &str,
    body: &Value,
) -> Result<Value, Fault> {
    match route {
        "/trust/inspect" => shared.session.project_trust(),
        "/configuration/wait" => Ok(json!(
            shared
                .session
                .wait_configuration(field(body, "operation")?)
                .await?
        )),
        "/manage/sessions" => {
            let directory = body["directory"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| Path::new(shared.session.cwd()).join(".eden/sessions"));
            Ok(json!(Session::list_saved_sessions(directory).await?))
        }
        "/manage/tree" => Ok(json!(
            Session::saved_session_tree(field(body, "path")?).await?
        )),
        _ => Err(fault("Unsupported", "unknown management read")),
    }
}
pub(super) async fn management_submit(
    shared: &Shared,
    route: &str,
    body: &Value,
) -> Result<Value, Fault> {
    match route {
        "/trust/save" => {
            shared
                .session
                .save_project_trust(&field::<PathBuf>(body, "path")?, field(body, "trusted")?)?;
            Ok(json!({ "saved": true, "effective": shared.session.project_trust()? }))
        }
        "/configuration/apply" => Ok(json!({
            "operation": shared
                    .session
                    .apply_configuration(field(body, "change")?, field(body, "mode")?)
                    .await?,
        })),
        "/delivery/save" => {
            shared
                .session
                .save_export_preview(&field::<String>(body, "preview_id")?, field(body, "path")?)
                .await?;
            Ok(json!({ "saved": body["path"] }))
        }
        "/manage/rename" => {
            let cli = shared
                .management
                .launch
                .as_ref()
                .ok_or_else(|| fault("Unavailable", "launch options unavailable"))?;
            let path = field::<PathBuf>(body, "path")?;
            let session = Session::open_saved_with_workspace(
                crate::composition(cli).map_err(|e| fault("InvalidInput", e.to_string()))?,
                path,
                None,
                crate::workspace_options(cli),
            )
            .await?;
            let result = async {
                let run = session.set_metadata(field(body, "name")?, field(body, "tags")?)?;
                session.wait(run).await?.into_result()
            }
            .await;
            let stopped = session.shutdown().await;
            let result = result?;
            stopped?;
            Ok(result)
        }
        "/manage/delete" => {
            if body["confirmed"] != true {
                return Err(fault(
                    "InvalidInput",
                    "confirm the exact selected session before deletion",
                ));
            }
            Session::delete_saved_session(field(body, "path")?, field(body, "expected_session")?)
                .await?;
            Ok(json!({ "deleted": body["path"] }))
        }
        "/manage/open" => {
            let mut cli = shared
                .management
                .launch
                .clone()
                .ok_or_else(|| fault("Unavailable", "host launch options unavailable"))?;
            let path = field::<PathBuf>(body, "path")?;
            if !path.is_file() {
                return Err(fault(
                    "InvalidInput",
                    "selected history file is unavailable",
                ));
            }
            cli.session = Some(path.clone());
            cli.cwd = None;
            cli.trust_project = false;
            cli.no_trust_project = false;
            cli.no_session = false;
            cli.model = None;
            cli.thinking = None;
            let reading = body["read_only"] == true;
            let endpoint = tokio::task::spawn_blocking(move || {
                crate::tui::start_host(&cli, reading.then_some(path.as_path()))
                    .map_err(|e| fault("HostLaunch", e.to_string()))
            })
            .await
            .map_err(|e| fault("HostLaunch", e.to_string()))??;
            Ok(json!({ "endpoint": endpoint }))
        }
        "/manage/copy/preview" => {
            let cli = shared
                .management
                .launch
                .as_ref()
                .ok_or_else(|| fault("Unavailable", "composition unavailable"))?;
            let composition =
                crate::composition(cli).map_err(|e| fault("InvalidInput", e.to_string()))?;
            let plan = Session::plan_copy(
                composition,
                CopyOptions {
                    source: field(body, "source")?,
                    destination: field(body, "destination")?,
                    kind: field(body, "kind")?,
                    target: body["target"].as_u64(),
                    cwd: None,
                    public_only: false,
                },
            )
            .await?;
            let id = token()?;
            let result = json!({ "preview_id": id, "plan": plan });
            let mut plans = shared
                .management
                .copies
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if plans.len() >= 8 {
                return Err(fault(
                    "PreviewLimit",
                    "discard a copy preview before creating another",
                ));
            }
            plans.insert(id, plan);
            Ok(result)
        }
        "/manage/copy/apply" => {
            if body["confirmed"] != true {
                return Err(fault("InvalidInput", "confirm the reviewed copy plan"));
            }
            let id = field::<String>(body, "preview_id")?;
            let plan = shared
                .management
                .copies
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id)
                .ok_or_else(|| fault("Unavailable", "copy preview expired"))?;
            Ok(json!({ "created": Session::apply_copy(plan).await? }))
        }
        "/manage/copy/discard" => {
            shared
                .management
                .copies
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&field::<String>(body, "preview_id")?);
            Ok(json!({ "discarded": true }))
        }
        "/delivery/preview" => {
            let (id, artifact) = shared
                .session
                .prepare_export(
                    field(body, "selection")?,
                    eden_protocol::delivery::Format::Jsonl,
                )
                .await?;
            Ok(json!({ "preview_id": id, "artifact": artifact }))
        }
        "/delivery/publish" => Ok(json!({
            "run_id": shared.session.publish_preview(
                &field::<String>(body, "preview_id")?,
                field(body, "confirmed")?
            )?,
        })),
        "/delivery/discard" => Ok(json!({
            "discarded": shared
                .session
                .forget_preview(&field::<String>(body, "preview_id")?),
        })),
        "/updates" => Ok(json!({ "run_id": shared.session.update(field(body, "request")?)? })),
        _ => Err(fault("Unsupported", "unknown management action")),
    }
}
