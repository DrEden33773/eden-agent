//! Pending dialogs are scoped to their invoking plugin operation, never to a transport reader.
use super::*;
use eden_protocol::interaction::{HOST, Interaction};
use serde_json::{Value, json};
use tokio::sync::oneshot;
struct Pending {
    kind: String,
    options: Vec<String>,
    reply: oneshot::Sender<Value>,
}
#[derive(Default)]
pub(crate) struct Interactions {
    enabled: std::sync::atomic::AtomicBool,
    next: AtomicU64,
    pending: Mutex<BTreeMap<u64, Pending>>,
}
struct PendingOwner {
    hub: Arc<Interactions>,
    id: u64,
    cx: eden_plugin_sdk::CallContext,
}
impl Drop for PendingOwner {
    fn drop(&mut self) {
        self.hub
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
        let _ = self
            .cx
            .emit("interaction_finished", json!({ "interaction_id": self.id }));
    }
}
impl Interactions {
    pub(crate) fn package(self: &Arc<Self>) -> eden_plugin_sdk::Package {
        let hub = self.clone();
        eden_plugin_sdk::Package::new("eden-host-interaction").service(
            HOST,
            move |input: Interaction, cx| {
                let hub = hub.clone();
                async move {
                    if !hub.enabled.load(Ordering::Acquire) {
                        return Err(Fault::new(
                            "Unsupported",
                            "interaction",
                            "no interaction handler attached",
                        ));
                    }
                    match input {
                        Interaction::Notify { kind, value } => {
                            if !["notify", "status", "widget", "title", "editor_text"]
                                .contains(&kind.as_str())
                            {
                                return Err(Fault::new(
                                    "InvalidInput",
                                    "interaction",
                                    "unknown notification kind",
                                ));
                            }
                            cx.emit(
                                "interaction_notification",
                                json!({ "kind": kind, "value": value }),
                            )?;
                            Ok(Value::Null)
                        }
                        Interaction::Request {
                            kind,
                            title,
                            options,
                            initial,
                            timeout_ms,
                        } => {
                            if !["select", "confirm", "input", "editor"].contains(&kind.as_str())
                                || (kind == "select" && options.is_empty())
                            {
                                return Err(Fault::new(
                                    "InvalidInput",
                                    "interaction",
                                    "invalid dialog kind or options",
                                ));
                            }
                            let id = hub.next.fetch_add(1, Ordering::Relaxed) + 1;
                            let (reply, receiver) = oneshot::channel();
                            {
                                let mut pending =
                                    hub.pending.lock().unwrap_or_else(|e| e.into_inner());
                                if !hub.enabled.load(Ordering::Acquire) {
                                    return Err(Fault::new(
                                        "Unsupported",
                                        "interaction",
                                        "handler detached before admission",
                                    ));
                                }
                                pending.insert(
                                    id,
                                    Pending {
                                        kind: kind.clone(),
                                        options: options.clone(),
                                        reply,
                                    },
                                );
                            }
                            let _owner = PendingOwner {
                                hub: hub.clone(),
                                id,
                                cx: cx.clone(),
                            };
                            cx.emit(
                                "interaction_requested",
                                json!({
                                    "interaction_id": id,
                                    "kind": kind,
                                    "title": title,
                                    "options": options,
                                    "initial": initial,
                                }),
                            )?;
                            match timeout_ms {
                                Some(ms) => tokio::time::timeout(
                                    std::time::Duration::from_millis(ms),
                                    receiver,
                                )
                                .await
                                .map_err(|_| {
                                    Fault::new("Timeout", "interaction", "interaction timed out")
                                })?,
                                None => receiver.await,
                            }
                            .map_err(|_| Fault::new("Cancelled", "interaction", "handler detached"))
                        }
                    }
                }
            },
        )
    }
}
impl Session {
    /// Attach an event-driven semantic handler. Disabling it cancels outstanding dialogs.
    /// Observe `interaction_requested` and answer with `respond_interaction`; subscription disposal
    /// alone does not detach the handler or close the shared session.
    pub fn set_interactions(&self, enabled: bool) {
        self.0
            .interactions
            .enabled
            .store(enabled, Ordering::Release);
        if !enabled {
            self.0
                .interactions
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        }
    }
    /// Resolve one outstanding dialog. Null dismisses it; duplicate, stale and malformed replies
    /// fail without answering another request. Authentication never passes through this method.
    pub fn respond_interaction(&self, id: u64, value: Value) -> Result<(), Fault> {
        let mut pending = self
            .0
            .interactions
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let request = pending.get(&id).ok_or_else(|| {
            Fault::new(
                "InvalidInput",
                "interaction",
                "unknown or expired interaction",
            )
        })?;
        let valid = value.is_null()
            || match request.kind.as_str() {
                "confirm" => value.is_boolean(),
                "select" => value
                    .as_str()
                    .is_some_and(|s| request.options.iter().any(|o| o == s)),
                _ => value.is_string(),
            };
        if !valid {
            return Err(Fault::new(
                "InvalidInput",
                "interaction",
                "invalid response value",
            ));
        }
        if let Some(request) = pending.remove(&id) {
            request
                .reply
                .send(value)
                .map_err(|_| Fault::new("Cancelled", "interaction", "dialog already cancelled"))?;
        }
        Ok(())
    }
}
