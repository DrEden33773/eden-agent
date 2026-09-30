//! Prompt acceptance and terminal observation are distinct, correlated lifecycle boundaries.
use crate::{Adapter, fault};
use eden_protocol::{Fault, Outcome};
use eden_tui_client::RequestStatus;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::time::Duration;

pub(crate) struct Prompt {
    id: String,
    run: Option<u64>,
    shell: bool,
    cancelled: bool,
    adopted: bool,
}
impl Adapter {
    pub(crate) async fn adopt_run(&self, snapshot: &eden_tui_client::Snapshot) -> Option<String> {
        let run = snapshot
            .state
            .active_run
            .or_else(|| snapshot.state.shell_runs.first().copied())?;
        let mut view = self.view.lock().await;
        let binding = view
            .prompts
            .entry(run)
            .or_insert_with(|| (format!("eden-{}-run-{run}", self.session), None));
        let id = binding.0.clone();
        let mut active = self.active_prompt.lock().await;
        if active.is_none() {
            *active = Some(Prompt {
                id: id.clone(),
                run: Some(run),
                shell: snapshot.state.shell_runs.contains(&run),
                cancelled: false,
                adopted: true,
            });
        }
        Some(id)
    }

    pub(crate) async fn finish_adopted(&self) {
        if self.load_pending.load(Ordering::Acquire) {
            return;
        }
        let owned = self
            .active_prompt
            .lock()
            .await
            .as_ref()
            .filter(|prompt| prompt.adopted)
            .and_then(|prompt| prompt.run.map(|run| (run, prompt.id.clone())));
        let Some((run, prompt_id)) = owned else {
            return;
        };
        {
            let view = self.view.lock().await;
            if view.snapshot.state.active_run == Some(run)
                || view.snapshot.state.shell_runs.contains(&run)
            {
                return;
            }
        }
        let Ok(terminal) = self.client.wait(run).await else {
            return;
        };
        let mut active = self.active_prompt.lock().await;
        if !active
            .as_ref()
            .is_some_and(|prompt| prompt.adopted && prompt.run == Some(run))
        {
            return;
        }
        *active = None;
        let failed =
            matches!(terminal.outcome, Outcome::Failed(_)) || !terminal.cleanup_errors.is_empty();
        self.send(json!({
            "method": "_x.ai/session/prompt_complete",
            "params": {
                "sessionId": self.identity,
                "promptId": prompt_id,
                "stopReason": if matches!(terminal.outcome, Outcome::Cancelled) {
                        "cancelled"
                    } else {
                        "end_turn"
                    },
                "agentResult": if failed {
                        Some("error")
                    } else {
                        None
                    },
                "_meta": { "edenRunId": run },
            },
        }));
    }

    pub(crate) async fn run_prompt(
        &self,
        params: &Value,
        body: &str,
        blocks: &[Value],
    ) -> Result<Value, Fault> {
        let request_id = self.request_id();
        let prompt_id = params["_meta"]["promptId"]
            .as_str()
            .unwrap_or(&request_id)
            .to_owned();
        let (route, mut request, shell) = if let Some(command) = blocks
            .iter()
            .find_map(|b| b["_meta"]["bash_command"].as_str())
        {
            (
                "/shell",
                json!({ "command": command, "shell": "bash", "exclude_from_context": false }),
                true,
            )
        } else if body.split_whitespace().next() == Some("/compact") {
            let instructions = body.strip_prefix("/compact").unwrap_or_default();
            (
                "/context/compact",
                json!({ "instructions": instructions.trim() }),
                false,
            )
        } else {
            let content = self.resource_content(body, blocks).await?;
            ("/prompt", json!({ "content": content }), false)
        };
        request["request_id"] = json!(request_id);
        {
            let mut active = self.active_prompt.lock().await;
            if active.is_some() {
                return Err(fault("An accepted or unresolved prompt is still active"));
            }
            *active = Some(Prompt {
                id: prompt_id.clone(),
                run: None,
                shell,
                cancelled: false,
                adopted: false,
            });
        }
        // The follow loop cannot publish an uncorrelated event between acceptance and registration.
        let mut view = self.view.lock().await;
        let submitted = self.post(route, request).await;
        let accepted = self.reconcile_acceptance(&request_id, submitted).await;
        let reply = match accepted {
            Ok(reply) => reply,
            Err(error) => {
                if error.code != "AcceptanceUnknown" {
                    *self.active_prompt.lock().await = None;
                }
                return Err(error);
            }
        };
        let run = reply["run_id"]
            .as_u64()
            .ok_or_else(|| fault("Missing accepted run identity"))?;
        let cancel = {
            let mut active = self.active_prompt.lock().await;
            let Some(prompt) = active.as_mut() else {
                return Err(fault("Prompt admission lost its owner"));
            };
            prompt.run = Some(run);
            prompt.cancelled
        };
        view.prompts
            .insert(run, (prompt_id.clone(), Some(request_id.clone())));
        if shell {
            self.shell_run.store(run, Ordering::Relaxed);
        }
        self.send(json!({
            "method": "_x.ai/queue/changed",
            "params": {
                "sessionId": self.identity,
                "entries": [],
                "runningPromptId": prompt_id,
                "_meta": { "edenRunId": run, "edenRequestId": request_id },
            },
        }));
        drop(view);
        if cancel {
            self.cancel_owned(run, shell).await?;
        }
        let terminal = self.client.wait(run).await?;
        let _ = self
            .shell_run
            .compare_exchange(run, 0, Ordering::Relaxed, Ordering::Relaxed);
        self.project(self.client.snapshot().await?, false).await;
        *self.active_prompt.lock().await = None;
        if let Some(error) = terminal.cleanup_errors.first() {
            return Err(error.clone());
        }
        if let Outcome::Failed(error) = &terminal.outcome {
            return Err(error.clone());
        }
        Ok(json!({
            "_meta": { "promptId": prompt_id, "edenRunId": run, "edenRequestId": request_id },
            "stopReason":
                if matches!(terminal.outcome, Outcome::Cancelled) {
                    "cancelled"
                } else {
                    "end_turn"
                },
        }))
    }

    async fn reconcile_acceptance(
        &self,
        request: &str,
        result: Result<Value, Fault>,
    ) -> Result<Value, Fault> {
        match result {
            Ok(reply) => return Ok(reply),
            Err(error)
                if !matches!(
                    error.code.as_str(),
                    "InputFailure" | "OutputFailure" | "Unavailable"
                ) =>
            {
                return Err(error);
            }
            Err(_) => {}
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match self.client.request_status(request).await {
                Ok(RequestStatus::Done { result }) => return result,
                Ok(RequestStatus::Unknown) => break,
                Ok(RequestStatus::Running) | Err(_) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(Fault::new(
            "AcceptanceUnknown",
            "grok-adapter",
            format!(
                "Acceptance of request {request} is unknown; reconnect and inspect the Session \
                 before resending"
            ),
        ))
    }

    async fn cancel_owned(&self, run: u64, shell: bool) -> Result<(), Fault> {
        if shell {
            self.client.cancel_shell(run).await
        } else {
            self.client.cancel(run).await
        }
    }

    pub(crate) async fn cancel_prompt(&self, params: &Value) -> Result<Value, Fault> {
        let requested = params["_meta"]["promptId"].as_str();
        {
            let mut active = self.active_prompt.lock().await;
            if let Some(prompt) = active.as_mut() {
                if requested.is_some_and(|id| id != prompt.id) {
                    return Ok(json!({ "ignored": "stale prompt" }));
                }
                prompt.cancelled = true;
                let owned = prompt.run.map(|run| (run, prompt.shell));
                drop(active);
                if let Some((run, shell)) = owned {
                    self.cancel_owned(run, shell).await?;
                }
                return Ok(json!({}));
            }
        }
        if requested.is_some() {
            return Ok(json!({ "ignored": "settled prompt" }));
        }
        // A fresh attachment may cancel work accepted before this adapter existed.
        let snapshot = self.client.snapshot().await?;
        if let Some(run) = snapshot.state.active_run {
            self.client.cancel(run).await?;
        }
        for run in snapshot.state.shell_runs {
            self.client.cancel_shell(run).await?;
        }
        Ok(json!({}))
    }
}
