//! Session-owned maintenance discovers updates without admitting a conversation run.
use super::*;
use eden_protocol::updates::{Channel, UPDATE_SOURCE, UpdateReply, UpdateRequest};
use serde_json::json;

/// Cancellation and its join barrier belong to the session, including failed delivery cleanup.
#[derive(Default)]
pub(crate) struct Maintenance {
    cancel: Cancellation,
    task: Mutex<Option<(tokio::task::JoinHandle<()>, Arc<Events>)>>,
}
impl Maintenance {
    pub(crate) async fn stop(&self) {
        self.cancel.cancel();
        let task = self.task.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some((task, events)) = task
            && let Err(error) = task.await
        {
            events.push(
                0,
                "update_check",
                json!({
                    "status": "failed",
                    "error":
                        Fault::new("CleanupFailure", "update_check", error.to_string()),
                }),
            );
        }
    }
}
impl Session {
    pub(crate) fn start_maintenance(&self) {
        let settings = match Workspace::discover(Path::new(self.cwd()), &self.0.workspace_options) {
            Ok(workspace) => workspace.settings,
            Err(error) => {
                self.0.events.push(
                    0,
                    "update_check",
                    json!({ "status": "failed", "error": error }),
                );
                return;
            }
        };
        let reason = if settings["offline_startup"].as_bool() == Some(true) {
            Some("offline_startup")
        } else if settings["update_check"].as_bool() == Some(false) {
            Some("disabled")
        } else {
            None
        };
        if let Some(reason) = reason {
            self.0.events.push(
                0,
                "update_check",
                json!({ "status": "skipped", "reason": reason }),
            );
            return;
        }
        let Ok(kernel) = self.0.kernel.get() else {
            return;
        };
        if !kernel.has_role(UPDATE_SOURCE) {
            return;
        }
        let events = self.0.events.clone();
        let cancel = self.0.maintenance.cancel.clone();
        let id = self.id();
        let task = tokio::spawn(async move {
            events.push(0, "update_check", json!({ "status": "checking" }));
            let result = check(&kernel, id, &cancel, &events).await;
            let payload = match result {
                Ok(()) => json!({ "status": "completed" }),
                Err(error) if error.code == "Cancelled" => json!({ "status": "cancelled" }),
                Err(error) => json!({ "status": "failed", "error": error }),
            };
            events.push(0, "update_check", payload);
        });
        *self
            .0
            .maintenance
            .task
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((task, self.0.events.clone()));
    }
}
async fn check(
    kernel: &Kernel,
    id: u64,
    cancel: &Cancellation,
    events: &Events,
) -> Result<(), Fault> {
    let reply = invoke(kernel, id, cancel, UpdateRequest::Discover).await?;
    let UpdateReply::Discovered { targets } = reply else {
        return Err(Fault::new(
            "InvalidReply",
            "update_check",
            "discovery returned an unexpected reply",
        ));
    };
    for target in targets {
        if cancel.is_cancelled() {
            return Err(Fault::new(
                "Cancelled",
                "update_check",
                "startup check cancelled",
            ));
        }
        if !target.configured {
            events.push(
                0,
                "update_check",
                json!({ "status": "unconfigured", "target": target.target }),
            );
            continue;
        }
        let reply = invoke(
            kernel,
            id,
            cancel,
            UpdateRequest::Check {
                target: target.target,
                channel: Channel::Stable,
            },
        )
        .await?;
        let UpdateReply::Checked { status } = reply else {
            return Err(Fault::new(
                "InvalidReply",
                "update_check",
                "check returned an unexpected reply",
            ));
        };
        events.push(
            0,
            "update_check",
            json!({ "status": "checked", "update": status }),
        );
    }
    Ok(())
}
async fn invoke(
    kernel: &Kernel,
    id: u64,
    cancel: &Cancellation,
    request: UpdateRequest,
) -> Result<UpdateReply, Fault> {
    let value = kernel
        .invoke(
            Request {
                execution: None,
                session_id: id,
                run_id: 0,
                contract: UPDATE_SOURCE.into(),
                payload: json!(request),
            },
            cancel.clone(),
        )
        .await
        .into_result()?;
    serde_json::from_value(value)
        .map_err(|e| Fault::new("InvalidReply", "update_check", e.to_string()))
}
