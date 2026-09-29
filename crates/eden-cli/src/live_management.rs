//! Launching another local host is an explicit management action; accepted work stays with its owner.
use super::*;
use eden_agent::{CopyOptions, CopyPlan};

#[derive(Default)]
pub(super) struct Management {
    pub(super) launch: Option<Cli>,
    pub(super) copies: Mutex<HashMap<String, CopyPlan>>,
    pub(super) scans: Mutex<HashMap<String, DirectoryReader>>,
}
pub(super) struct DirectoryReader {
    attachment: u64,
    reader: Arc<tokio::sync::Mutex<eden_agent::SavedSessionScan>>,
}
fn managed_path(shared: &Shared, body: &Value, key: &str) -> Result<PathBuf, Fault> {
    let path: PathBuf = field(body, key)?;
    Ok(if path.is_absolute() {
        path
    } else {
        Path::new(shared.session.cwd()).join(path)
    })
}
pub(super) async fn management_read(
    shared: &Shared,
    route: &str,
    body: &Value,
) -> Result<Value, Fault> {
    match route {
        "/manage/sessions/start" => {
            let attachment = field::<u64>(body, "attachment")?;
            shared.session.presentation_heartbeat(attachment)?;
            let directory = body["directory"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|path| Path::new(shared.session.cwd()).join(path))
                .unwrap_or_else(|| Path::new(shared.session.cwd()).join(".eden/sessions"));
            let id = token()?;
            let scan = Session::scan_saved_sessions(directory);
            shared
                .management
                .scans
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    id.clone(),
                    DirectoryReader {
                        attachment,
                        reader: Arc::new(tokio::sync::Mutex::new(scan)),
                    },
                );
            Ok(json!({ "scan_id": id }))
        }
        "/manage/sessions/poll" => {
            let id = field::<String>(body, "scan_id")?;
            let scan = shared
                .management
                .scans
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id)
                .map(|entry| entry.reader.clone())
                .ok_or_else(|| fault("Cancelled", "directory scan closed"))?;
            let mut scan = scan.lock().await;
            let mut entries = vec![];
            if let Ok(Some(entry)) = tokio::time::timeout(Duration::from_secs(4), scan.next()).await
            {
                entries.push(entry?);
            }
            while let Some(entry) = scan.try_next() {
                entries.push(entry?);
            }
            let done = scan.finished();
            drop(scan);
            if done {
                shared
                    .management
                    .scans
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
            }
            Ok(json!({ "scan_id": id, "entries": entries, "done": done }))
        }
        "/manage/sessions/cancel" => {
            let id = field::<String>(body, "scan_id")?;
            let scan = shared
                .management
                .scans
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            if let Some(entry) = scan {
                entry.reader.lock().await.cancel_and_wait().await;
            }
            Ok(json!({ "cancelled": true }))
        }
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
                .map(|path| Path::new(shared.session.cwd()).join(path))
                .unwrap_or_else(|| Path::new(shared.session.cwd()).join(".eden/sessions"));
            Ok(json!(Session::list_saved_sessions(directory).await?))
        }
        "/manage/tree" => Ok(json!(
            Session::saved_session_tree(managed_path(shared, body, "path")?).await?
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
            shared.session.save_project_trust(
                &managed_path(shared, body, "path")?,
                field(body, "trusted")?,
            )?;
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
                .save_export_preview(
                    &field::<String>(body, "preview_id")?,
                    managed_path(shared, body, "path")?,
                )
                .await?;
            Ok(json!({ "saved": body["path"] }))
        }
        "/manage/rename" => {
            let cli = shared
                .management
                .launch
                .as_ref()
                .ok_or_else(|| fault("Unavailable", "launch options unavailable"))?;
            let path = managed_path(shared, body, "path")?;
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
            Session::delete_saved_session(
                managed_path(shared, body, "path")?,
                field(body, "expected_session")?,
            )
            .await?;
            Ok(json!({ "deleted": body["path"] }))
        }
        "/manage/open" => {
            let mut cli = shared
                .management
                .launch
                .clone()
                .ok_or_else(|| fault("Unavailable", "host launch options unavailable"))?;
            let path = managed_path(shared, body, "path")?;
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
            if !reading && let Some(endpoint) = crate::tui::selected_live_endpoint(&path).await {
                return Ok(json!({ "endpoint": endpoint, "attached_existing": true }));
            }
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
                    source: managed_path(shared, body, "source")?,
                    destination: managed_path(shared, body, "destination")?,
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

pub(super) fn detach_scans(shared: &Shared, attachment: u64) {
    let mut scans = shared
        .management
        .scans
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    scans.retain(|_, entry| entry.attachment != attachment);
}
