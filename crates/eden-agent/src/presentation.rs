//! Session-owned live views and frontend attachments.
use super::*;
use eden_protocol::presentation as p;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::{Duration, Instant},
};
use tokio::sync::{oneshot, watch};

type ViewKey = (String, String);
#[derive(Default)]
struct HubState {
    sequence: u64,
    next_revision: u64,
    next_attachment: u64,
    views: BTreeMap<ViewKey, p::LiveView>,
    attachments: BTreeMap<u64, (String, Instant)>,
    activity: BTreeMap<u64, (p::ActivityTarget, Instant)>,
    active_runs: BTreeSet<u64>,
    draining_runs: BTreeSet<u64>,
    actions: BTreeMap<String, ActionState>,
    claimed_forms: BTreeMap<(String, String, u64, String), String>,
    done_order: VecDeque<String>,
    closed: bool,
    shared: bool,
}
enum ActionState {
    Running {
        run_id: u64,
        request: p::ActionRequest,
        listeners: Vec<oneshot::Sender<Result<Value, Fault>>>,
    },
    Done {
        request: p::ActionRequest,
        result: Result<Value, Fault>,
    },
}
pub(crate) struct Hub {
    state: Mutex<HubState>,
    changed: watch::Sender<u64>,
}
impl Default for Hub {
    fn default() -> Self {
        Self {
            state: Mutex::new(HubState::default()),
            changed: watch::channel(0).0,
        }
    }
}
impl Hub {
    pub(crate) fn invalidate_instances(&self, owners: &[String]) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for ((owner, _), view) in &mut state.views {
            if owners.contains(owner) {
                view.active = false;
            }
        }
        self.bump(&mut state);
    }
    fn bump(&self, state: &mut HubState) {
        state.sequence += 1;
        self.changed.send_replace(state.sequence);
    }
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
    pub(crate) fn publish(
        &self,
        owner: &str,
        run_id: u64,
        view: p::View,
    ) -> Result<p::Revision, Fault> {
        validate_view(&view)?;
        if owner.is_empty() {
            return Err(Fault::new(
                "InvalidInput",
                "presentation",
                "missing plugin owner",
            ));
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed || !state.active_runs.contains(&run_id) {
            return Err(Fault::new(
                "Unavailable",
                "presentation",
                "owning run is no longer active",
            ));
        }
        if !state.shared && state.attachments.is_empty() && contains_action(&view.nodes) {
            return Err(Fault::new(
                "Unsupported",
                "presentation",
                "no live frontend can handle this action",
            ));
        }
        if matches!(
            view.slot,
            p::Slot::Header | p::Slot::Footer | p::Slot::Overlay | p::Slot::Composer
        ) && state.views.values().any(|existing| {
            existing.active
                && existing.view.slot == view.slot
                && (existing.owner != owner || existing.view.id != view.id)
        }) {
            return Err(Fault::new(
                "SlotConflict",
                "presentation",
                "exclusive slot already occupied",
            ));
        }
        let key = (owner.to_owned(), view.id.clone());
        let revision = state
            .next_revision
            .checked_add(1)
            .ok_or_else(|| Fault::new("Unavailable", "presentation", "revision space exhausted"))?;
        state.next_revision = revision;
        state
            .claimed_forms
            .retain(|(owner, id, _, _), _| owner != &key.0 || id != &key.1);
        state.views.insert(
            key,
            p::LiveView {
                owner: owner.into(),
                run_id,
                revision,
                active: true,
                handled_actions: vec![],
                view,
            },
        );
        self.bump(&mut state);
        Ok(p::Revision { value: revision })
    }
    pub(crate) fn remove(&self, owner: &str, run_id: u64, id: &str) -> Result<(), Fault> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let key = (owner.to_owned(), id.to_owned());
        if state
            .views
            .get(&key)
            .is_none_or(|view| view.run_id != run_id || !view.active)
        {
            return Err(Fault::new(
                "Unavailable",
                "presentation",
                "view is not owned by this run",
            ));
        }
        state.views.remove(&key);
        state
            .claimed_forms
            .retain(|(owner, id, _, _), _| owner != &key.0 || id != &key.1);
        self.bump(&mut state);
        Ok(())
    }
    pub(crate) fn set_shared(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).shared = true;
    }
    pub(crate) fn begin_run(&self, run_id: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active_runs.insert(run_id);
    }
    pub(crate) fn end_run(&self, run_id: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active_runs.remove(&run_id);
        state.draining_runs.remove(&run_id);
        let mut changed = false;
        for view in state.views.values_mut() {
            if view.run_id == run_id && view.active {
                view.active = false;
                changed = true;
            }
        }
        if changed {
            self.bump(&mut state);
        }
    }
    /// Close action admission before cancellation, then wait for SDK cleanup and cached results.
    /// Views remain publishable during cleanup so the final static snapshot includes its updates.
    pub(crate) async fn drain_actions(&self, run_id: u64, cancel: &Cancellation) {
        let mut changed = self.changed.subscribe();
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.draining_runs.insert(run_id);
        }
        cancel.cancel();
        loop {
            changed.borrow_and_update();
            let running = self.state.lock().unwrap_or_else(|e| e.into_inner()).actions.values()
                .any(|action| matches!(action, ActionState::Running { run_id: owner, .. } if *owner == run_id));
            if !running {
                return;
            }
            if changed.changed().await.is_err() {
                return;
            }
        }
    }
    fn expire_attachment(&self, attachment: u64, timestamp: Instant) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .attachments
            .get(&attachment)
            .is_some_and(|(_, current)| *current == timestamp)
        {
            state.attachments.remove(&attachment);
            state.activity.remove(&attachment);
            self.bump(&mut state);
        }
    }
    fn expire_activity(&self, attachment: u64, timestamp: Instant) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .activity
            .get(&attachment)
            .is_some_and(|(_, current)| *current == timestamp)
        {
            state.activity.remove(&attachment);
            self.bump(&mut state);
        }
    }
    // A saved view inherits its source record's selection class. Resolve a missing
    // sequence from the run's latest matching content; a terminal is only a
    // fallback for results that have no separate committed tool/extension node.
    pub(crate) fn static_record(&self, run_id: u64, records: &[c::Record]) -> p::StaticRecord {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let views = state
            .views
            .values()
            .filter(|view| view.run_id == run_id)
            .filter_map(|view| {
                let mut view = view.clone();
                let source = view.view.source.as_mut()?;
                let origin = match source.record_sequence {
                    Some(sequence) => records.iter().find(|record| record.sequence == sequence),
                    None => records
                        .iter()
                        .rev()
                        .find(|record| {
                            record.run_id == run_id
                                && record.kind != "terminal"
                                && p::source_record_matches(source.class, record)
                        })
                        .or_else(|| {
                            records.iter().rev().find(|record| {
                                record.run_id == run_id
                                    && record.kind == "terminal"
                                    && p::source_record_matches(source.class, record)
                            })
                        }),
                }?;
                if origin.run_id != run_id || !p::source_record_matches(source.class, origin) {
                    return None;
                }
                let sequence = origin.sequence;
                source.record_sequence = Some(sequence);
                view.active = false;
                serde_json::to_value(view).ok()
            })
            .collect();
        p::StaticRecord {
            version: p::VERSION,
            views,
        }
    }
    pub(crate) fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        state.active_runs.clear();
        state.attachments.clear();
        state.activity.clear();
        for view in state.views.values_mut() {
            view.active = false;
        }
        self.bump(&mut state);
    }
    fn snapshot(&self, session_id: u64) -> p::Snapshot {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        p::Snapshot {
            version: p::VERSION,
            session_id,
            sequence: state.sequence,
            views: state.views.values().cloned().collect(),
            activity: state
                .activity
                .iter()
                .filter_map(|(id, (target, _))| {
                    state.attachments.get(id).map(|(frontend, _)| p::Activity {
                        attachment: *id,
                        frontend: frontend.clone(),
                        target: target.clone(),
                    })
                })
                .collect(),
            pending_interactions: state
                .views
                .values()
                .filter(|view| {
                    view.active
                        && view.owner == "eden-host-interaction"
                        && view.view.id.starts_with("dialog-")
                })
                .map(|view| json!({ "view_id": view.view.id, "title": view.view.title }))
                .collect(),
        }
    }
}
fn contains_action(nodes: &[p::Node]) -> bool {
    nodes.iter().any(|node| match node {
        p::Node::Form { .. } | p::Node::Button { .. } => true,
        p::Node::Group { children, .. } => contains_action(children),
        _ => false,
    })
}
fn validate_view(view: &p::View) -> Result<(), Fault> {
    if view.id.is_empty() || view.title.is_empty() || view.fallback.is_empty() {
        return Err(Fault::new(
            "InvalidInput",
            "presentation",
            "view identity, title and fallback are required",
        ));
    }
    if view
        .source
        .as_ref()
        .and_then(|source| source.record_sequence)
        == Some(0)
    {
        return Err(Fault::new(
            "InvalidInput",
            "presentation",
            "source record sequence must be positive",
        ));
    }
    let mut platforms = BTreeSet::new();
    if view
        .platforms
        .iter()
        .any(|platform| !["tui", "web"].contains(&platform.as_str()) || !platforms.insert(platform))
    {
        return Err(Fault::new(
            "InvalidInput",
            "presentation",
            "platform names must be unique supported frontend names",
        ));
    }
    if !view.platforms.is_empty() && view.nodes.is_empty() {
        return Err(Fault::new(
            "InvalidInput",
            "presentation",
            "platform contribution needs a portable fallback node",
        ));
    }
    fn visit(
        nodes: &[p::Node],
        seen: &mut BTreeSet<String>,
        actions: &mut BTreeSet<String>,
    ) -> bool {
        nodes.iter().all(|node| {
            if node.id().is_empty() || !seen.insert(node.id().to_owned()) {
                return false;
            }
            match node {
                p::Node::Group { children, .. } => visit(children, seen, actions),
                p::Node::Form { action, fields, .. } => {
                    let mut names = BTreeSet::new();
                    !action.is_empty()
                        && actions.insert(action.clone())
                        && !fields.is_empty()
                        && fields.iter().all(|field| {
                            let valid_options = match field.kind {
                                p::FieldKind::Choice | p::FieldKind::MultiChoice => {
                                    !field.options.is_empty()
                                        && field.options.iter().all(|option| !option.is_empty())
                                }
                                _ => field.options.is_empty(),
                            };
                            field.id.len() <= 128
                                && !field.id.is_empty()
                                && !field.label.is_empty()
                                && names.insert(&field.id)
                                && valid_options
                                && field.options.iter().collect::<BTreeSet<_>>().len()
                                    == field.options.len()
                        })
                }
                p::Node::Button { action, .. } => {
                    !action.is_empty() && actions.insert(action.clone())
                }
                p::Node::Attachment {
                    record_sequence, ..
                } => *record_sequence > 0,
                _ => true,
            }
        })
    }
    if !visit(&view.nodes, &mut BTreeSet::new(), &mut BTreeSet::new()) {
        return Err(Fault::new(
            "InvalidInput",
            "presentation",
            "duplicate or invalid semantic node, action, field or source",
        ));
    }
    Ok(())
}
fn find_action<'a>(nodes: &'a [p::Node], action: &str) -> Option<&'a p::Node> {
    for node in nodes {
        match node {
            p::Node::Form { action: name, .. } | p::Node::Button { action: name, .. }
                if name == action =>
            {
                return Some(node);
            }
            p::Node::Group { children, .. } => {
                if let Some(found) = find_action(children, action) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}
fn validate_action(node: &p::Node, values: &Value) -> Result<(), Fault> {
    let p::Node::Form { fields, .. } = node else {
        return if values.is_null() || values.as_object().is_some_and(|v| v.is_empty()) {
            Ok(())
        } else {
            Err(Fault::new(
                "InvalidInput",
                "presentation",
                "button takes no form values",
            ))
        };
    };
    let object = values.as_object().ok_or_else(|| {
        Fault::new(
            "InvalidInput",
            "presentation",
            "form values must be an object",
        )
    })?;
    if object
        .keys()
        .any(|key| !fields.iter().any(|field| field.id == *key))
    {
        return Err(Fault::new(
            "InvalidInput",
            "presentation",
            "unknown form field",
        ));
    }
    for field in fields {
        let value = object.get(&field.id);
        if value.is_none() || value == Some(&Value::Null) {
            if field.required {
                return Err(Fault::new(
                    "InvalidInput",
                    "presentation",
                    "required form field is missing",
                ));
            }
            continue;
        }
        let value = value.expect("checked above");
        let valid = match field.kind {
            p::FieldKind::Text => value
                .as_str()
                .is_some_and(|text| !field.required || !text.trim().is_empty()),
            p::FieldKind::Choice => value
                .as_str()
                .is_some_and(|option| field.options.iter().any(|candidate| candidate == option)),
            p::FieldKind::MultiChoice => value.as_array().is_some_and(|items| {
                (!field.required || !items.is_empty())
                    && items.iter().all(|item| {
                        item.as_str().is_some_and(|option| {
                            field.options.iter().any(|candidate| candidate == option)
                        })
                    })
            }),
            p::FieldKind::Boolean => value.is_boolean(),
        };
        if !valid {
            return Err(Fault::new(
                "InvalidInput",
                "presentation",
                "invalid form value",
            ));
        }
    }
    Ok(())
}
impl Session {
    /// Keep the live Session and its pending presentation usable while every frontend is detached.
    /// Only an explicitly started shared host calls this; ordinary embedding retains headless behavior.
    pub fn enable_shared_presentation(&self) {
        self.0.presentation.set_shared();
    }
    /// Attach a named live frontend. Detaching never cancels a run or a waiting dialog.
    pub fn attach_presentation(&self, frontend: &str) -> Result<u64, Fault> {
        if !["tui", "web"].contains(&frontend) {
            return Err(Fault::new(
                "Unsupported",
                "presentation",
                "unknown frontend",
            ));
        }
        let hub = &self.0.presentation;
        let mut state = hub.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err(Fault::new("Unavailable", "presentation", "session closed"));
        }
        state.next_attachment += 1;
        let id = state.next_attachment;
        let timestamp = Instant::now();
        state
            .attachments
            .insert(id, (frontend.to_owned(), timestamp));
        hub.bump(&mut state);
        drop(state);
        let hub = hub.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            hub.expire_attachment(id, timestamp);
        });
        Ok(id)
    }
    /// Refresh an attached frontend lease. A crashed frontend is detached after ten seconds;
    /// its run and any pending question remain owned by the Session.
    pub fn presentation_heartbeat(&self, attachment: u64) -> Result<(), Fault> {
        let hub = &self.0.presentation;
        let mut state = hub.state.lock().unwrap_or_else(|e| e.into_inner());
        let current = state
            .attachments
            .get_mut(&attachment)
            .ok_or_else(|| Fault::new("InvalidInput", "presentation", "unknown attachment"))?;
        let timestamp = Instant::now();
        current.1 = timestamp;
        drop(state);
        let hub = hub.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            hub.expire_attachment(attachment, timestamp);
        });
        Ok(())
    }
    /// Detach one frontend and clear its transient input activity.
    pub fn detach_presentation(&self, attachment: u64) -> Result<(), Fault> {
        let hub = &self.0.presentation;
        let mut state = hub.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.attachments.remove(&attachment).is_none() {
            return Err(Fault::new(
                "InvalidInput",
                "presentation",
                "unknown attachment",
            ));
        }
        state.activity.remove(&attachment);
        hub.bump(&mut state);
        Ok(())
    }
    /// Report activity for the main composer or a shared, non-secret form.
    pub fn presentation_activity(
        &self,
        attachment: u64,
        target: p::ActivityTarget,
        active: bool,
    ) -> Result<(), Fault> {
        let hub = &self.0.presentation;
        let mut state = hub.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.attachments.contains_key(&attachment) {
            return Err(Fault::new(
                "InvalidInput",
                "presentation",
                "unknown attachment",
            ));
        }
        if active {
            if let p::ActivityTarget::Form {
                owner,
                view_id,
                node_id,
            } = &target
            {
                let Some(view) = state.views.get(&(owner.clone(), view_id.clone())) else {
                    return Err(Fault::new("InvalidInput", "presentation", "unknown form"));
                };
                if !view.active || !contains_form(&view.view.nodes, node_id) {
                    return Err(Fault::new("InvalidInput", "presentation", "inactive form"));
                }
            }
            let timestamp = Instant::now();
            state.activity.insert(attachment, (target, timestamp));
            hub.bump(&mut state);
            drop(state);
            let hub = hub.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(4)).await;
                hub.expire_activity(attachment, timestamp);
            });
            return Ok(());
        }
        state.activity.remove(&attachment);
        hub.bump(&mut state);
        Ok(())
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
    /// Wait for a newer live sequence. Consumers that miss a window take another snapshot.
    pub async fn presentation_changed(&self, sequence: u64) {
        let mut receiver = self.0.presentation.changed.subscribe();
        while *receiver.borrow_and_update() <= sequence {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
    /// Admit one owner-scoped action. The Session owns execution after admission, so
    /// losing the calling connection does not lose its result or cause a retry to execute twice.
    pub async fn presentation_action(&self, request: p::ActionRequest) -> Result<Value, Fault> {
        if request.session_id != self.id() {
            return Err(Fault::new(
                "SessionMismatch",
                "presentation",
                "foreign session",
            ));
        }
        if request.request_id.is_empty() || request.request_id.len() > 256 {
            return Err(Fault::new(
                "InvalidInput",
                "presentation",
                "invalid request id",
            ));
        }
        let hub = &self.0.presentation;
        let (send, receive) = oneshot::channel();
        let start = {
            let mut state = hub.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = state.actions.get_mut(&request.request_id) {
                match existing {
                    ActionState::Done {
                        request: prior,
                        result,
                    } => {
                        return if *prior == request {
                            result.clone()
                        } else {
                            Err(Fault::new(
                                "DuplicateId",
                                "presentation",
                                "request id reused with different action",
                            ))
                        };
                    }
                    ActionState::Running {
                        request: prior,
                        listeners,
                        ..
                    } => {
                        if *prior != request {
                            return Err(Fault::new(
                                "DuplicateId",
                                "presentation",
                                "request id reused with different action",
                            ));
                        }
                        listeners.push(send);
                        None
                    }
                }
            } else {
                let key = (request.owner.clone(), request.view_id.clone());
                let view = state.views.get(&key).ok_or_else(|| {
                    Fault::new("Unavailable", "presentation", "view does not exist")
                })?;
                if !view.active || state.closed || state.draining_runs.contains(&view.run_id) {
                    return Err(Fault::new(
                        "Unavailable",
                        "presentation",
                        "view is no longer active",
                    ));
                }
                if view.revision != request.revision {
                    return Err(Fault::new(
                        "StaleRevision",
                        "presentation",
                        "view changed; refresh before submitting",
                    ));
                }
                let admitted_run_id = view.run_id;
                let handler = if request.owner == "eden-host-interaction" {
                    None
                } else {
                    Some(
                        self.0
                            .kernel
                            .get()?
                            .instance_service(&request.owner, p::ACTION)?,
                    )
                };
                let node = find_action(&view.view.nodes, &request.action)
                    .ok_or_else(|| Fault::new("InvalidInput", "presentation", "unknown action"))?;
                validate_action(node, &request.values)?;
                if matches!(node, p::Node::Form { .. }) {
                    let claim = (
                        request.owner.clone(),
                        request.view_id.clone(),
                        request.revision,
                        request.action.clone(),
                    );
                    if state.claimed_forms.contains_key(&claim) {
                        return Err(Fault::new(
                            "AlreadyHandled",
                            "presentation",
                            "form already answered",
                        ));
                    }
                    state
                        .claimed_forms
                        .insert(claim, request.request_id.clone());
                    if let Some(view) = state.views.get_mut(&key) {
                        view.handled_actions.push(request.action.clone());
                    }
                    hub.bump(&mut state);
                }
                state.actions.insert(
                    request.request_id.clone(),
                    ActionState::Running {
                        run_id: admitted_run_id,
                        request: request.clone(),
                        listeners: vec![send],
                    },
                );
                Some((admitted_run_id, handler))
            }
        };
        if let Some((admitted_run_id, handler)) = start {
            let session = self.clone();
            tokio::spawn(async move {
                session
                    .complete_presentation_action(request, admitted_run_id, handler)
                    .await;
            });
        }
        receive
            .await
            .map_err(|_| Fault::new("Unavailable", "presentation", "action result lost"))?
    }
    async fn complete_presentation_action(
        &self,
        request: p::ActionRequest,
        admitted_run_id: u64,
        handler: Option<Arc<eden_kernel::ServiceHandle>>,
    ) {
        let hub = &self.0.presentation;
        let owner = {
            let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            let view = hub
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .views
                .get(&(request.owner.clone(), request.view_id.clone()))
                .cloned();
            match view {
                Some(view)
                    if !state.closed
                        && view.active
                        && view.run_id == admitted_run_id
                        && view.revision == request.revision =>
                {
                    let cancel = state
                        .active
                        .as_ref()
                        .filter(|(run, _)| *run == view.run_id)
                        .map(|(_, cancel)| cancel)
                        .or_else(|| state.commands.get(&view.run_id));
                    cancel
                        .map(|cancel| (view.run_id, cancel.clone()))
                        .ok_or_else(|| {
                            Fault::new("Unavailable", "presentation", "owning run has settled")
                        })
                }
                _ => Err(Fault::new(
                    "Unavailable",
                    "presentation",
                    "owning run has settled",
                )),
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
        let mut state = hub.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ActionState::Running { listeners, .. }) =
            state.actions.remove(&request.request_id)
        {
            for listener in listeners {
                let _ = listener.send(result.clone());
            }
        }
        if result.is_err() {
            state
                .claimed_forms
                .retain(|_, owner| owner != &request.request_id);
            if let Some(view) = state
                .views
                .get_mut(&(request.owner.clone(), request.view_id.clone()))
                && view.revision == request.revision
            {
                view.handled_actions
                    .retain(|action| action != &request.action);
                hub.bump(&mut state);
            }
        }
        state.done_order.push_back(request.request_id.clone());
        state.actions.insert(
            request.request_id.clone(),
            ActionState::Done { request, result },
        );
        hub.bump(&mut state);
        while state.done_order.len() > 1024 {
            if let Some(old) = state.done_order.pop_front() {
                state.actions.remove(&old);
            }
        }
    }
}
fn contains_form(nodes: &[p::Node], id: &str) -> bool {
    nodes.iter().any(|node| match node {
        p::Node::Form { id: found, .. } => found == id,
        p::Node::Group { children, .. } => contains_form(children, id),
        _ => false,
    })
}
