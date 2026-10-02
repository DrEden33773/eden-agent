//! Launching another local host is an explicit management action; accepted work stays with its owner.
use super::*;

#[derive(Default)]
pub(super) struct Management {
    pub(super) inspection: tokio::sync::Mutex<()>,
    pub(super) launch: Option<Cli>,
    pub(super) previews: Mutex<previews::Previews>,
    pub(super) scans: Mutex<HashMap<String, DirectoryReader>>,
}
pub(super) struct DirectoryReader {
    attachment: u64,
    reader: Arc<tokio::sync::Mutex<eden_agent::SavedSessionScan>>,
}
pub(super) fn managed_path(shared: &Shared, body: &Value, key: &str) -> Result<PathBuf, Fault> {
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
            let directory = body["directory"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|path| Path::new(shared.session.cwd()).join(path))
                .unwrap_or_else(|| Path::new(shared.session.cwd()).join(".eden/sessions"));
            let id = token()?;
            let mut scans = shared
                .management
                .scans
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // Admission and publication share the cleanup map lock: detach cannot miss
            // a reader admitted immediately before its lease ends.
            shared.session.presentation_heartbeat(attachment)?;
            let scan = Session::scan_saved_sessions(directory);
            scans.insert(
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
            if done {
                scan.cancel_and_wait().await;
            }
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
            let reader = shared
                .management
                .scans
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id)
                .map(|entry| entry.reader.clone());
            if let Some(reader) = reader {
                reader.lock().await.cancel_and_wait().await;
                shared
                    .management
                    .scans
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
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
        "/manage/info" => {
            let path = managed_path(shared, body, "path")?;
            let mut info = json!(Session::describe_saved_session(path.clone()).await?);
            info["owner"] = json!(crate::tui::selected_live_endpoint(&path).await?);
            info["status"] = json!(if info["owner"].is_null() {
                "stopped; binding checked on open"
            } else {
                "live writer"
            });
            Ok(info)
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
        "/delivery/save" => previews::save(shared, body).await,
        "/manage/rename" => {
            let cli = shared
                .management
                .launch
                .as_ref()
                .ok_or_else(|| fault("Unavailable", "launch options unavailable"))?;
            let runtime =
                crate::tui::lifecycle(cli).map_err(|e| fault("Unavailable", e.to_string()))?;
            let tags = if body["preserve_tags"] == true {
                None
            } else {
                Some(field(body, "tags")?)
            };
            runtime
                .rename_history(
                    &managed_path(shared, body, "path")?,
                    field(body, "name")?,
                    tags,
                )
                .await?;
            Ok(json!("Session metadata saved."))
        }
        "/manage/delete" => {
            if body["confirmed"] != true {
                return Err(fault(
                    "InvalidInput",
                    "confirm the exact selected session before deletion",
                ));
            }
            let cli = shared
                .management
                .launch
                .as_ref()
                .ok_or_else(|| fault("Unavailable", "launch options unavailable"))?;
            let runtime =
                crate::tui::lifecycle(cli).map_err(|e| fault("Unavailable", e.to_string()))?;
            let plan = runtime
                .plan_removal(&managed_path(shared, body, "path")?)
                .await?;
            if plan.history.session_id != Some(field::<u64>(body, "expected_session")?) {
                return Err(fault("StaleSelection", "history identity changed"));
            }
            // Compatibility callers cannot infer permission to stop an executing owner.
            let receipt = runtime.remove_history(plan, false).await?;
            Ok(json!({ "deleted": body["path"], "trash": receipt }))
        }
        "/manage/new" => {
            let mut cli = shared
                .management
                .launch
                .clone()
                .ok_or_else(|| fault("Unavailable", "host launch options unavailable"))?;
            cli.session = None;
            let cwd = body["cwd"]
                .as_str()
                .filter(|cwd| !cwd.is_empty())
                .unwrap_or(shared.session.cwd());
            if cwd != shared.session.cwd() {
                cli.trust_project = false;
                cli.no_trust_project = false;
            }
            cli.cwd = Some(PathBuf::from(cwd));
            cli.no_session = body["management_only"] == true;
            let lifecycle =
                crate::tui::lifecycle(&cli).map_err(|e| fault("StartFailed", e.to_string()))?;
            let opened = lifecycle.create(!cli.no_session).await?;
            let endpoint = opened.endpoint;
            Ok(json!({ "endpoint": endpoint }))
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
            let lifecycle =
                crate::tui::lifecycle(&cli).map_err(|e| fault("StartFailed", e.to_string()))?;
            let opened = lifecycle.open(&path, reading).await?;
            Ok(json!(opened))
        }
        "/manage/copy/preview" => previews::prepare_copy(shared, body).await,
        "/manage/copy/apply" => previews::apply_copy(shared, body).await,
        "/manage/copy/discard" => previews::discard(shared, body, true),
        "/delivery/preview" => previews::prepare_export(shared, body).await,
        "/delivery/publish" => previews::publish(shared, body),
        "/delivery/discard" => previews::discard(shared, body, false),
        "/updates" => Ok(json!({ "run_id": shared.session.update(field(body, "request")?)? })),
        _ => Err(fault("Unsupported", "unknown management action")),
    }
}

pub(super) async fn detach_scans(shared: &Shared, attachment: u64) {
    let readers = shared
        .management
        .scans
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(_, entry)| entry.attachment == attachment)
        .map(|(id, entry)| (id.clone(), entry.reader.clone()))
        .collect::<Vec<_>>();
    for (id, reader) in readers {
        // Keep the shared barrier discoverable until it has joined the producer.
        // Concurrent cancel, expiry and explicit detach wait on this same reader.
        reader.lock().await.cancel_and_wait().await;
        shared
            .management
            .scans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn concurrent_owner_cleanup_waits_for_the_same_scan_worker() {
        let session = super::super::tests::session().await;
        let (stop, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            instance: "test-instance".into(),
            management: Default::default(),
            session: session.clone(),
            token: "fixture".into(),
            web_root: None,
            submissions: Mutex::new((HashMap::new(), VecDeque::new())),
            history: tokio::sync::Mutex::new(None),
            presentation_history: tokio::sync::Mutex::new(None),
            stop,
        });
        let attachment = session.attach_presentation("tui").unwrap();
        let directory =
            std::env::temp_dir().join(format!("eden-scan-barrier-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        for index in 0..40 {
            std::fs::write(directory.join(format!("{index}.jsonl")), b"{bad").unwrap();
        }
        let scan = management_read(
            &shared,
            "/manage/sessions/start",
            &json!({ "attachment": attachment, "directory": directory }),
        )
        .await
        .unwrap();
        let id = scan["scan_id"].as_str().unwrap();
        let reader = shared
            .management
            .scans
            .lock()
            .unwrap()
            .get(id)
            .unwrap()
            .reader
            .clone();
        let held = reader.lock().await;
        let mut observer = Box::pin(detach_scans(&shared, attachment));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(observer.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let mut explicit = Box::pin(dispatch(
            &shared,
            "POST",
            "/detach",
            json!({ "attachment": attachment }),
        ));
        let early = tokio::time::timeout(Duration::from_millis(20), &mut explicit).await;
        let acknowledged_early = early.is_ok();
        drop(held);
        observer.await;
        match early {
            Ok(result) => result.unwrap(),
            Err(_) => explicit.await.unwrap(),
        };
        session.shutdown().await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(
            !acknowledged_early,
            "detach bypassed an already-closing scan"
        );
    }
}
