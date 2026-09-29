//! Thin native package and Session adapters for the independent presentation runtime.
use super::*;
use eden_protocol::presentation as p;
use serde_json::{Value, json};

#[derive(Default)]
pub(crate) struct Hub(Arc<eden_presentation::Hub>);
impl std::ops::Deref for Hub {
    type Target = Arc<eden_presentation::Hub>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Hub {
    pub(crate) fn package(self: &Arc<Self>) -> eden_plugin_sdk::Package {
        let hub = self.clone();
        eden_plugin_sdk::Package::new("eden-host-presentation").service(
            p::HOST,
            move |request: p::HostRequest, cx| {
                let hub = hub.clone();
                async move {
                    match request {
                        p::HostRequest::Publish { owner, view } => hub
                            .publish(&owner, cx.run_id(), view)
                            .map(|revision| json!(revision)),
                        p::HostRequest::Remove { owner, id } => {
                            hub.remove(&owner, cx.run_id(), &id)?;
                            Ok(Value::Null)
                        }
                    }
                }
            },
        )
    }
    pub(crate) async fn drain_actions(&self, run_id: u64, cancel: &Cancellation) {
        self.0.begin_drain(run_id);
        cancel.cancel();
        self.0.wait_for_actions(run_id).await;
    }
}
impl Session {
    /// Keep pending presentation usable while every frontend is detached from a shared host.
    pub fn enable_shared_presentation(&self) {
        self.0.presentation.set_shared();
    }
    /// Attach a named live frontend without transferring ownership of runs or dialogs.
    pub fn attach_presentation(&self, frontend: &str) -> Result<u64, Fault> {
        self.0.presentation.attach(frontend)
    }
    /// Keep the frontend lease live; a crashed frontend detaches after ten seconds.
    pub fn presentation_heartbeat(&self, attachment: u64) -> Result<(), Fault> {
        self.0.presentation.heartbeat(attachment)
    }
    /// Await expiry or explicit detach so adapters can dispose only frontend-owned work.
    pub async fn wait_presentation_detach(&self, attachment: u64) {
        self.0.presentation.wait_for_detach(attachment).await;
    }
    /// Detach a frontend and clear its transient input activity without cancelling its run.
    pub fn detach_presentation(&self, attachment: u64) -> Result<(), Fault> {
        self.0.presentation.detach(attachment)
    }
    /// Report shared input activity without transmitting field values.
    pub fn presentation_activity(
        &self,
        attachment: u64,
        target: p::ActivityTarget,
        active: bool,
    ) -> Result<(), Fault> {
        self.0.presentation.activity(attachment, target, active)
    }
    /// Commit the last source-bound views after a run's terminal record settles.
    pub(crate) async fn persist_static_presentation(&self, run_id: u64) -> Result<(), Fault> {
        if !self.0.coding || !self.0.kernel.available() {
            return Ok(());
        }
        let records = self.history().await?;
        let saved = self.0.presentation.static_record(run_id, &records);
        if !saved.views.is_empty() {
            self.commit(run_id, "presentation_static", serde_json::json!(saved))
                .await?;
        }
        Ok(())
    }
    /// Capture one atomic live sequence with its current views and activity.
    pub fn presentation_snapshot(&self) -> p::Snapshot {
        self.0.presentation.snapshot(self.id())
    }
    /// Wait for a newer live sequence; consumers can recover missed updates with a snapshot.
    pub async fn presentation_changed(&self, sequence: u64) {
        self.0.presentation.changed(sequence).await;
    }
    /// Admit an owner-scoped action whose execution and cached result survive frontend detachment.
    pub async fn presentation_action(&self, request: p::ActionRequest) -> Result<Value, Fault> {
        let mut admission = self.0.presentation.admit_action(self.id(), &request, || {
            if request.owner == "eden-host-interaction"
                || request.owner == crate::configuration_presentation::OWNER
            {
                Ok(None)
            } else {
                self.0
                    .kernel
                    .get()?
                    .instance_service(&request.owner, p::ACTION)
                    .map(Some)
            }
        })?;
        if let Some((admitted_run_id, handler)) = admission.take_dispatch() {
            let session = self.clone();
            tokio::spawn(async move {
                session
                    .complete_presentation_action(request, admitted_run_id, handler)
                    .await;
            });
        }
        admission.result().await
    }
    async fn complete_presentation_action(
        &self,
        request: p::ActionRequest,
        admitted_run_id: u64,
        handler: Option<Arc<eden_kernel::ServiceHandle>>,
    ) {
        let hub = &self.0.presentation;
        if request.owner == crate::configuration_presentation::OWNER {
            let result = if hub.action_is_current(&request, admitted_run_id) {
                self.configuration_presentation_action(&request).await
            } else {
                Err(Fault::new(
                    "Unavailable",
                    "presentation",
                    "management view changed",
                ))
            };
            hub.complete_action(request, result);
            return;
        }
        let owner = {
            let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            if !state.closed && hub.action_is_current(&request, admitted_run_id) {
                let cancel = state
                    .active
                    .as_ref()
                    .filter(|(run, _)| *run == admitted_run_id)
                    .map(|(_, cancel)| cancel)
                    .or_else(|| state.commands.get(&admitted_run_id));
                cancel
                    .map(|cancel| (admitted_run_id, cancel.clone()))
                    .ok_or_else(|| {
                        Fault::new("Unavailable", "presentation", "owning run has settled")
                    })
            } else {
                Err(Fault::new(
                    "Unavailable",
                    "presentation",
                    "owning run has settled",
                ))
            }
        };
        let result = match owner {
            Ok((_run_id, _cancel)) if request.owner == "eden-host-interaction" => {
                let id = request
                    .view_id
                    .strip_prefix("dialog-")
                    .and_then(|value| value.parse::<u64>().ok());
                match id {
                    Some(id) => {
                        let value = if request.action == "dismiss" {
                            Value::Null
                        } else {
                            request.values.get("value").cloned().unwrap_or(Value::Null)
                        };
                        self.respond_interaction(id, value)
                            .map(|_| json!({ "delivered": true }))
                    }
                    None => Err(Fault::new(
                        "InvalidInput",
                        "presentation",
                        "invalid dialog identity",
                    )),
                }
            }
            Ok((run_id, cancel)) => match handler {
                Some(handler) => handler
                    .call(
                        Request {
                            execution: None,
                            session_id: self.id(),
                            run_id,
                            contract: p::ACTION.into(),
                            payload: json!(request),
                        },
                        cancel,
                    )
                    .await
                    .into_result(),
                None => Err(Fault::new(
                    "Unavailable",
                    "presentation",
                    "missing action generation",
                )),
            },
            Err(error) => Err(error),
        };
        hub.complete_action(request, result);
    }
}
