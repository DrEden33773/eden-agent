//! Snapshot admission and cancellation share one lock so invalidation cannot miss a replay.
use super::*;

#[derive(Default)]
pub(crate) struct AuxiliaryGate {
    state: Mutex<State>,
    changed: tokio::sync::Notify,
}
#[derive(Default)]
struct State {
    revision: u64,
    next: u64,
    active: BTreeMap<u64, (Cancellation, Option<u64>)>,
    streaming_closed_through: u64,
}
pub(crate) struct Lease<'a> {
    gate: &'a AuxiliaryGate,
    id: u64,
}
impl AuxiliaryGate {
    pub fn revision(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .revision
    }
    pub fn invalidate(&self) -> u64 {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.revision += 1;
        for (cancel, _) in state.active.values() {
            cancel.cancel();
        }
        state.revision
    }
    pub fn admit(
        &self,
        revision: u64,
        cancel: Cancellation,
        streaming_run: Option<u64>,
    ) -> Result<Lease<'_>, Fault> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if revision != state.revision
            || cancel.is_cancelled()
            || streaming_run.is_some_and(|run| run <= state.streaming_closed_through)
        {
            return Err(Fault::new(
                "StaleSnapshot",
                "auxiliary",
                "prepared context is no longer current",
            ));
        }
        state.next += 1;
        let id = state.next;
        state.active.insert(id, (cancel, streaming_run));
        Ok(Lease { gate: self, id })
    }
    pub fn stop_streaming(&self, run: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.streaming_closed_through = state.streaming_closed_through.max(run);
        for (cancel, owner) in state.active.values() {
            if *owner == Some(run) {
                cancel.cancel();
            }
        }
    }
    pub async fn settle_streaming(&self, run: u64) {
        self.settle_matching(Some(run)).await;
    }
    pub async fn settle(&self) {
        self.settle_matching(None).await;
    }
    async fn settle_matching(&self, run: Option<u64>) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .active
                .values()
                .any(|(_, owner)| run.is_none() || *owner == run)
            {
                return;
            }
            changed.await;
        }
    }
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.gate
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active
            .remove(&self.id);
        self.gate.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn streaming_settlement_preserves_idle_but_closes_late_admission() {
        let gate = AuxiliaryGate::default();
        let stream = Cancellation::default();
        let idle = Cancellation::default();
        let lease = gate.admit(0, stream.clone(), Some(9)).unwrap();
        let idle_lease = gate.admit(0, idle.clone(), None).unwrap();
        gate.stop_streaming(9);
        assert!(stream.is_cancelled());
        assert!(!idle.is_cancelled());
        assert!(gate.admit(0, Cancellation::default(), Some(9)).is_err());
        drop(lease);
        gate.settle_streaming(9).await;
        assert!(!idle.is_cancelled());
        drop(idle_lease);
    }
    #[tokio::test]
    async fn invalidation_cancels_replay_and_waits_for_cleanup() {
        let gate = AuxiliaryGate::default();
        let revision = gate.revision();
        let cancel = Cancellation::default();
        let lease = gate.admit(revision, cancel.clone(), None).unwrap();
        gate.invalidate();
        assert!(cancel.is_cancelled());
        assert!(gate.admit(revision, Cancellation::default(), None).is_err());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), gate.settle())
                .await
                .is_err()
        );
        drop(lease);
        gate.settle().await;
        assert!(
            gate.admit(gate.revision(), Cancellation::default(), None)
                .is_ok()
        );
    }
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    #[tokio::test]
    async fn current_observation_position_survives_ledger_eviction() {
        let events = Events::new(1);
        events.push(7, "accepted", serde_json::json!({ "management": false }));
        for _ in 0..8300 {
            events.push(7, "delta", serde_json::Value::Null);
        }
        assert_eq!(events.read_after(0).await.unwrap_err().code, "Lagged");
        let position = events.event_position();
        assert_eq!(position.foreground_run, Some(7));
        events.push(7, "settled", serde_json::Value::Null);
        let batch = events.read_after(position.cursor).await.unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].kind, "settled");
        assert_eq!(events.event_position().foreground_run, None);
    }
}
