//! Draft publication and work admission share the existing store's serial writer.
use super::*;

/// A consumer can release a draft while its first work is still being checked.
pub enum DraftCleanup {
    /// Preflight or another consumer still owns the draft; the host must keep watching.
    Pending,
    /// Publication already transferred ownership to persistent history.
    Retained,
    /// Admission is sealed and the unused draft is ready for ordered shutdown.
    Closed,
}

impl Session {
    /// Seal only unsaved, idle, unattached drafts. The admission lock also protects
    /// consumer attachment, so neither new work nor a new consumer can race retirement.
    pub fn prepare_draft_cleanup(&self) -> DraftCleanup {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.0.draft_path.is_none() || self.history_path().is_some() || state.closed {
            return DraftCleanup::Retained;
        }
        if state.active.is_some()
            || !state.shells.is_empty()
            || !state.commands.is_empty()
            || state.management
            || state.pending_inputs != 0
            || self.0.presentation.has_attachments()
        {
            return DraftCleanup::Pending;
        }
        state.closed = true;
        DraftCleanup::Closed
    }

    /// Keep cleanup in the host after its creating consumer exits during preflight.
    /// Publication retains the host; a rejected admission retires only after every
    /// consumer has detached and all work has settled.
    pub async fn wait_draft_cleanup(&self) -> bool {
        loop {
            let settled = self.0.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            let sequence = self.presentation_snapshot().sequence;
            match self.prepare_draft_cleanup() {
                DraftCleanup::Closed => return true,
                DraftCleanup::Retained => return false,
                DraftCleanup::Pending => {}
            }
            tokio::select! {
                _ = settled => {},
                _ = self.presentation_changed(sequence) => {}
            }
        }
    }

    pub(crate) fn restore_store_request(&self, records: Vec<c::Record>) -> c::StoreRequest {
        if let Some(path) = self.history_path() {
            c::StoreRequest::Open {
                path: Some(path.to_string_lossy().into_owned()),
                session_id: self.id(),
            }
        } else if let Some(path) = &self.0.draft_path {
            c::StoreRequest::OpenDraft {
                path: path.to_string_lossy().into_owned(),
                session_id: self.id(),
                records,
            }
        } else {
            c::StoreRequest::RestoreMemory {
                session_id: self.id(),
                records,
            }
        }
    }

    pub(crate) async fn save_intent(
        &self,
        run_id: u64,
        intent: serde_json::Value,
        cancel: &Cancellation,
    ) -> Result<(), Fault> {
        if cancel.is_cancelled() {
            return Err(Fault::new(
                "Cancelled",
                "session-admission",
                "work cancelled before admission",
            ));
        }
        if self.0.coding {
            let timestamp_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| Fault::new("Unavailable", "session-admission", e.to_string()))?
                .as_millis();
            let entries = vec![c::RecordDraft {
                kind: "work_admitted".into(),
                payload: serde_json::json!({ "intent": intent, "timestamp_ms": timestamp_ms }),
            }];
            let request = if self.0.draft_path.is_some() {
                c::StoreRequest::AdmitBatch { run_id, entries }
            } else {
                c::StoreRequest::AppendBatch { run_id, entries }
            };
            // This owned store operation is not interrupted after publication starts. Its
            // receipt decides admission even when the requesting frontend has gone away.
            self.service::<_, c::StoreReply>(run_id, c::STORE, &request)
                .await?;
            if let Some(path) = &self.0.draft_path
                && self.0.history_path.set(path.clone()).is_ok()
            {
                self.0
                    .events
                    .push(run_id, "session_saved", serde_json::json!({ "path": path }));
            }
        }
        Ok(())
    }
    pub(crate) async fn admit_work(
        &self,
        run_id: u64,
        intent: serde_json::Value,
        cancel: &Cancellation,
    ) -> Result<(), Fault> {
        self.save_intent(run_id, intent, cancel).await?;
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .admissions
            .insert(run_id, Ok(()));
        self.0.events.push(
            run_id,
            "accepted",
            serde_json::json!({ "management": false }),
        );
        self.0.settled.notify_waiters();
        Ok(())
    }

    /// Await work acceptance, not its final result. A draft's success proves that its
    /// identity and original intent are saved before business execution can begin.
    pub async fn wait_admission(&self, run_id: u64) -> Result<(), Fault> {
        loop {
            let changed = self.0.settled.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(result) = state.admissions.get(&run_id) {
                    return result.clone();
                }
                if state.active.as_ref().map(|(id, _)| *id) != Some(run_id)
                    && !state.shells.contains_key(&run_id)
                    && !state.commands.contains_key(&run_id)
                {
                    return Err(Fault::new(
                        "InvalidInput",
                        "session-admission",
                        "unknown work admission",
                    ));
                }
            }
            changed.await;
        }
    }
}
