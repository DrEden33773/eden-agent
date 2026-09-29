//! Shared headless control operations, independent of transport framing.
use super::*;
use serde::Serialize;
/// Admission and lifecycle state; a cancellation acknowledgement is not a settled result.
#[derive(Clone, Debug, Serialize)]
#[allow(missing_docs)]
pub struct SessionState {
    pub session_id: u64,
    pub cwd: String,
    pub closed: bool,
    pub active_run: Option<u64>,
    pub managing: bool,
    pub shell_runs: Vec<u64>,
    pub command_runs: Vec<u64>,
    pub pending_inputs: usize,
}
impl Session {
    /// Inspect admission without contacting a provider or waiting for an active run.
    pub fn state(&self) -> SessionState {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        SessionState {
            session_id: self.id(),
            cwd: self.cwd().into(),
            closed: state.closed,
            active_run: state.active.as_ref().map(|(id, _)| *id),
            managing: state.management,
            shell_runs: state.shells.keys().copied().collect(),
            command_runs: state.commands.keys().copied().collect(),
            pending_inputs: state.pending_inputs,
        }
    }
    /// Read exactly the tool catalog the default loop advertises after selection and exclusions.
    pub async fn tools(&self) -> Result<Vec<c::ToolDefinition>, Fault> {
        let reply: eden_protocol::resources::Catalog = self
            .service(
                0,
                eden_protocol::resources::TOOL_CATALOG,
                &eden_protocol::resources::CatalogRequest {
                    cwd: self.cwd().into(),
                },
            )
            .await?;
        Ok(reply.tools)
    }
    /// Retract waiting queue entries on the current branch. Already-delivered input is not
    /// retractable; cancel its run first so restoration returns it to the pending queue.
    pub async fn withdraw_queue(&self, ids: Option<Vec<u64>>) -> Result<Vec<c::QueueEntry>, Fault> {
        let run_id = self.admit_input()?;
        let session = self.clone();
        let owner = InputOwner(session.clone());
        tokio::spawn(async move {
            let _owner = owner;
            session
                .service_input(run_id, c::QUEUE, &c::QueueRequest::Withdraw { ids })
                .await
        })
        .await
        .map_err(|e| Fault::new("Unavailable", "queue", e.to_string()))?
    }
    /// Change live automatic maintenance policy or stop only the current retry backoff.
    /// These switches belong to this package instance and are not disk configuration edits.
    pub async fn control(
        &self,
        request: c::CodingControlRequest,
    ) -> Result<c::CodingControlState, Fault> {
        let run_id = self.admit_input()?;
        let session = self.clone();
        let owner = InputOwner(session.clone());
        tokio::spawn(async move {
            let _owner = owner;
            session
                .service_input(run_id, c::CODING_CONTROL, &request)
                .await
        })
        .await
        .map_err(|e| Fault::new("Unavailable", "control", e.to_string()))?
    }
    /// Inspect original and edited model input without claiming an edit or starting a model.
    pub async fn inspect_context(&self) -> Result<eden_protocol::context_edit::Snapshot, Fault> {
        self.context_operation(|input| eden_protocol::context_edit::Request::Inspect { input })
            .await
    }
    /// Atomically accept an edit at the reviewed revision, including while a run is active.
    /// The coding loop uses it only at the next model request boundary.
    pub async fn edit_context(
        &self,
        edit: eden_protocol::context_edit::Apply,
    ) -> Result<eden_protocol::context_edit::Snapshot, Fault> {
        self.context_operation(move |input| eden_protocol::context_edit::Request::Apply {
            input,
            edit,
        })
        .await
    }
    /// Reconstruct original context on a new branch; this never replays tools or calls a model.
    pub fn rebuild_context(
        &self,
        rebuild: eden_protocol::context_edit::Rebuild,
    ) -> Result<u64, Fault> {
        self.start(true, move |session, run_id, cancel| async move {
            let result = async {
                let input = c::ContextInput {
                    target: session.inspect_model_target().await?,
                    resources: session.context_resources().await?,
                    tools: session.context_tools().await?,
                    action: "inspect".into(),
                    records: vec![],
                    instructions: String::new(),
                    limits: c::ModelLimits::default(),
                    cwd: session.cwd().into(),
                    items: vec![],
                };
                let snapshot: eden_protocol::context_edit::Snapshot = session
                    .service_with_cancel(
                        run_id,
                        eden_protocol::context_edit::SERVICE,
                        &eden_protocol::context_edit::Request::Rebuild { input, rebuild },
                        cancel,
                    )
                    .await?;
                Ok::<_, Fault>(serde_json::json!(snapshot))
            }
            .await;
            Terminal {
                outcome: match result {
                    Ok(value) => Outcome::Completed(value),
                    Err(error) => Outcome::Failed(error),
                },
                cleanup_errors: vec![],
                partial_result: None,
            }
        })
    }
    /// Explicitly preserve, omit or re-adapt a reviewed image while retaining all prior versions.
    pub async fn edit_images(
        &self,
        edit: eden_protocol::context_edit::ImageEdit,
    ) -> Result<eden_protocol::context_edit::Snapshot, Fault> {
        self.context_operation(move |input| eden_protocol::context_edit::Request::Images {
            input,
            edit,
        })
        .await
    }
    /// Check references and image compatibility against current model input before admitting a draft.
    /// Sending still rechecks at its actual boundary if context or configuration changes.
    pub async fn check_reference_input(
        &self,
        content: Vec<c::Block>,
        references: Vec<eden_protocol::session_reference::Reference>,
    ) -> Result<eden_protocol::context_edit::Snapshot, Fault> {
        self.context_operation(
            move |input| eden_protocol::context_edit::Request::CheckInput {
                input,
                content,
                references,
            },
        )
        .await
    }
    async fn context_operation(
        &self,
        build: impl FnOnce(c::ContextInput) -> eden_protocol::context_edit::Request + Send + 'static,
    ) -> Result<eden_protocol::context_edit::Snapshot, Fault> {
        let run_id = self.admit_input()?;
        let session = self.clone();
        let owner = InputOwner(session.clone());
        tokio::spawn(async move {
            let _owner = owner;
            let input = c::ContextInput {
                target: session.inspect_model_target().await?,
                resources: session.context_resources().await?,
                tools: session.context_tools().await?,
                action: "inspect".into(),
                records: vec![],
                instructions: String::new(),
                limits: c::ModelLimits::default(),
                cwd: session.cwd().into(),
                items: vec![],
            };
            let request = build(input);
            session
                .service_input(run_id, eden_protocol::context_edit::SERVICE, &request)
                .await
        })
        .await
        .map_err(|error| Fault::new("Unavailable", "context-edit", error.to_string()))?
    }
    pub(crate) fn admit_input(&self) -> Result<u64, Fault> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed || state.management {
            return Err(Fault::new(
                "Unavailable",
                "session",
                "session closed or managing history",
            ));
        }
        self.configuration_admission(c::CODING_CONTROL, false)?;
        state.pending_inputs += 1;
        Ok(state.active.as_ref().map(|(id, _)| *id).unwrap_or(0))
    }
}
impl Session {
    pub(crate) fn start_command(
        &self,
        name: String,
        arguments: serde_json::Value,
    ) -> Result<u64, Fault> {
        self.start_control_operation(
            eden_protocol::resources::COMMAND,
            serde_json::json!(eden_protocol::resources::CommandRequest {
                cwd: self.cwd().into(),
                name: name.clone(),
                arguments
            }),
            serde_json::json!({ "command": name }),
            Ok,
        )
    }
    pub(crate) fn start_control_operation(
        &self,
        contract: &str,
        payload: serde_json::Value,
        accepted: serde_json::Value,
        project: fn(serde_json::Value) -> Result<serde_json::Value, Fault>,
    ) -> Result<u64, Fault> {
        let contract = contract.to_owned();
        self.0.kernel.get()?;
        let (run_id, cancel) = {
            let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed || state.management {
                return Err(Fault::new(
                    "Unavailable",
                    "command",
                    "session closed or managing history",
                ));
            }
            self.configuration_admission(&contract, false)?;
            let id = state.next;
            state.next = state
                .next
                .checked_add(1)
                .ok_or_else(|| Fault::new("Unavailable", "command", "run identity exhausted"))?;
            let cancel = Cancellation::default();
            state.commands.insert(id, cancel.clone());
            state.command_contracts.insert(id, contract.clone());
            self.0.presentation.begin_run(id);
            (id, cancel)
        };
        self.0.events.push(run_id, "accepted", accepted);
        let session = self.clone();
        tokio::spawn(async move {
            let mut terminal = session
                .0
                .kernel
                .invoke(
                    Request {
                        execution: None,
                        session_id: session.id(),
                        run_id,
                        contract,
                        payload,
                    },
                    cancel.clone(),
                )
                .await;
            session.0.presentation.drain_actions(run_id, &cancel).await;
            if let Outcome::Completed(value) = &mut terminal.outcome {
                terminal.outcome = match project(std::mem::take(value)) {
                    Ok(value) => Outcome::Completed(value),
                    Err(error) => Outcome::Failed(error),
                };
            }
            if session.0.coding
                && let Err(error) = session
                    .commit(run_id, "terminal", serde_json::json!(terminal))
                    .await
            {
                terminal.cleanup_errors.push(error);
            }
            if let Err(error) = session.persist_static_presentation(run_id).await {
                terminal.cleanup_errors.push(error);
            }
            let mut state = session.0.state.lock().unwrap_or_else(|e| e.into_inner());
            state.commands.remove(&run_id);
            state.command_contracts.remove(&run_id);
            session.0.presentation.end_run(run_id);
            state.terminals.insert(run_id, terminal.clone());
            session
                .0
                .events
                .push(run_id, "settled", serde_json::json!(terminal));
            session.0.settled.notify_waiters();
        });
        Ok(run_id)
    }
}
