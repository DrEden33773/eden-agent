//! Explicit shell operations have independent cancellation while sharing session shutdown.
use super::*;
use eden_protocol::shell::{ShellRequest, USER_SHELL};
use serde_json::json;
impl Session {
    /// Start an explicit user command, even while a model run is active. Output events carry this
    /// run identity. Its history record is appended after the active model run settles to preserve
    /// tool-call/result ordering; `exclude_from_context` affects projection, not public history.
    pub fn user_shell(
        &self,
        command: String,
        shell: String,
        exclude_from_context: bool,
    ) -> Result<u64, Fault> {
        self.0.kernel.get()?;
        if command.is_empty() || !["bash", "powershell"].contains(&shell.as_str()) {
            return Err(Fault::new(
                "InvalidInput",
                "user-shell",
                "provide a command and bash or powershell",
            ));
        }
        let (run_id, cancel) = {
            let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed || state.management {
                return Err(Fault::new(
                    "Unavailable",
                    "user-shell",
                    "session closed or managing history",
                ));
            }
            let id = state.next;
            state.next = state
                .next
                .checked_add(1)
                .ok_or_else(|| Fault::new("Unavailable", "user-shell", "run identity exhausted"))?;
            let cancel = Cancellation::default();
            state.shells.insert(id, cancel.clone());
            (id, cancel)
        };
        self.0
            .events
            .push(run_id, "accepted", json!({ "user_shell": true }));
        let session = self.clone();
        tokio::spawn(async move {
            let mut terminal = session
                .0
                .kernel
                .invoke(
                    Request {
                        session_id: session.id(),
                        run_id,
                        contract: USER_SHELL.into(),
                        payload: json!(ShellRequest {
                            cwd: session.cwd().into(),
                            command: command.clone(),
                            shell: shell.clone()
                        }),
                    },
                    cancel,
                )
                .await;
            // New model submissions are excluded while a shell owns a deferred history record.
            // The model run admitted before this shell remains the only possible predecessor.
            let active = session
                .0
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .active
                .as_ref()
                .map(|(id, _)| *id);
            if let Some(active) = active
                && let Err(error) = session.wait(active).await
            {
                terminal.cleanup_errors.push(error);
            }
            let result =
                terminal
                    .partial_result
                    .clone()
                    .unwrap_or_else(|| match &terminal.outcome {
                        Outcome::Completed(value) => value.clone(),
                        Outcome::Failed(error) => {
                            json!({
                                "text": "",
                                "error": error,
                                "exit_code": null,
                                "truncated": false,
                            })
                        }
                        Outcome::Cancelled => {
                            json!({
                                "text": "",
                                "error":
                                    Fault::new("Cancelled", "user-shell", "command cancelled"),
                                "exit_code": null,
                                "truncated": false,
                            })
                        }
                    });
            if session.0.coding {
                if let Err(error) = session
                    .commit(
                        run_id,
                        "user_shell",
                        json!({
                            "command": command,
                            "shell": shell,
                            "exclude_from_context": exclude_from_context,
                            "result": result,
                        }),
                    )
                    .await
                {
                    terminal.cleanup_errors.push(error);
                }
                if let Err(error) = session.commit(run_id, "terminal", json!(terminal)).await {
                    terminal.cleanup_errors.push(error);
                }
            }
            let mut state = session.0.state.lock().unwrap_or_else(|e| e.into_inner());
            state.shells.remove(&run_id);
            state.terminals.insert(run_id, terminal.clone());
            session.0.events.push(run_id, "settled", json!(terminal));
            session.0.settled.notify_waiters();
        });
        Ok(run_id)
    }
    /// Cancel only this explicit shell. `wait` remains the process and history completion barrier.
    pub fn cancel_shell(&self, run_id: u64) -> Result<(), Fault> {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cancel) = state.shells.get(&run_id) {
            cancel.cancel();
            return Ok(());
        }
        if state.terminals.contains_key(&run_id) {
            return Ok(());
        }
        Err(Fault::new(
            "InvalidInput",
            "user-shell",
            "unknown shell run",
        ))
    }
}
