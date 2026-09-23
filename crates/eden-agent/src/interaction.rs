//! Pending dialogs are scoped to their invoking plugin operation, never to a transport reader.
use super::*;
use eden_protocol::interaction::{HOST, Interaction};
use eden_protocol::presentation as p;
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
    presentation: Arc<super::presentation::Hub>,
    view_id: Option<String>,
}
impl Drop for PendingOwner {
    fn drop(&mut self) {
        self.hub
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
        if let Some(view_id) = &self.view_id {
            let _ = self
                .presentation
                .remove("eden-host-interaction", self.cx.run_id(), view_id);
        }
        let _ = self
            .cx
            .emit("interaction_finished", json!({ "interaction_id": self.id }));
    }
}
impl Interactions {
    pub(crate) fn package(
        self: &Arc<Self>,
        presentation: Arc<super::presentation::Hub>,
    ) -> eden_plugin_sdk::Package {
        let hub = self.clone();
        eden_plugin_sdk::Package::new("eden-host-interaction").service(
            HOST,
            move |input: Interaction, cx| {
                let hub = hub.clone();
                let presentation = presentation.clone();
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
                            let slot = match kind.as_str() {
                                "title" => p::Slot::Header,
                                "status" => p::Slot::Footer,
                                "editor_text" => p::Slot::Composer,
                                _ => p::Slot::Panel,
                            };
                            let text = value
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| value.to_string());
                            let view = p::View::new(
                                format!("notification-{kind}"),
                                slot,
                                format!("Plugin {kind}"),
                            )
                            .node(p::Node::Status {
                                id: "value".into(),
                                text,
                            });
                            let _ =
                                presentation.publish("eden-host-interaction", cx.run_id(), view);
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
                            let view_id = format!("dialog-{id}");
                            let field = match kind.as_str() {
                                "select" => p::Field {
                                    id: "value".into(),
                                    label: title.clone(),
                                    kind: p::FieldKind::Choice,
                                    required: true,
                                    initial: Some(json!(initial)),
                                    options: options.clone(),
                                },
                                "confirm" => p::Field {
                                    id: "value".into(),
                                    label: title.clone(),
                                    kind: p::FieldKind::Boolean,
                                    required: true,
                                    initial: None,
                                    options: vec![],
                                },
                                _ => p::Field {
                                    id: "value".into(),
                                    label: title.clone(),
                                    kind: p::FieldKind::Text,
                                    required: false,
                                    initial: Some(json!(initial)),
                                    options: vec![],
                                },
                            };
                            let view = p::View::new(view_id.clone(), p::Slot::Panel, title.clone())
                                .node(p::Node::Form {
                                    id: "reply".into(),
                                    action: "respond".into(),
                                    fields: vec![field],
                                })
                                .node(p::Node::Button {
                                    id: "dismiss".into(),
                                    action: "dismiss".into(),
                                    label: "Dismiss".into(),
                                });
                            let published = presentation
                                .publish("eden-host-interaction", cx.run_id(), view)
                                .is_ok();
                            let _owner = PendingOwner {
                                hub: hub.clone(),
                                id,
                                cx: cx.clone(),
                                presentation: presentation.clone(),
                                view_id: published.then_some(view_id),
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
