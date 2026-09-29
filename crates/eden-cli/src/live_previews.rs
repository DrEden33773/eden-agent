//! Loopback previews belong to the attachment that prepared them; execution outlives UI ownership.
use super::*;
use eden_agent::{CopyPlan, delivery::save_artifact_file};
use eden_plugin_sdk::Cancellation;
use eden_protocol::delivery::{Artifact, Format, PublishRequest};
use std::collections::HashSet;

#[derive(Default)]
pub(super) struct Previews {
    closed: HashSet<u64>,
    exports: HashMap<String, (Option<u64>, Artifact)>,
    copies: HashMap<String, (Option<u64>, CopyPlan)>,
    preparing: HashMap<String, Pending>,
}
struct Pending {
    attachment: Option<u64>,
    cancel: Cancellation,
    finished: watch::Receiver<bool>,
}
struct Preparation<'a> {
    shared: &'a Shared,
    id: String,
    attachment: Option<u64>,
    cancel: Cancellation,
    finished: watch::Sender<bool>,
}
impl Drop for Preparation<'_> {
    fn drop(&mut self) {
        self.shared
            .management
            .previews
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .preparing
            .remove(&self.id);
        self.finished.send_replace(true);
    }
}
fn begin<'a>(shared: &'a Shared, body: &Value) -> Result<Preparation<'a>, Fault> {
    let attachment = body
        .get("attachment")
        .map(|_| field(body, "attachment"))
        .transpose()?;
    if let Some(attachment) = attachment {
        shared.session.presentation_heartbeat(attachment)?;
    }
    let mut previews = shared
        .management
        .previews
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if attachment.is_some_and(|id| previews.closed.contains(&id)) {
        return Err(fault("Cancelled", "preview owner detached"));
    }
    let id = field::<String>(body, "request_id")?;
    let cancel = Cancellation::default();
    let (finished, receiver) = watch::channel(false);
    previews.preparing.insert(
        id.clone(),
        Pending {
            attachment,
            cancel: cancel.clone(),
            finished: receiver,
        },
    );
    Ok(Preparation {
        shared,
        id,
        attachment,
        cancel,
        finished,
    })
}
fn active(previews: &Previews, preparation: &Preparation<'_>) -> Result<(), Fault> {
    if preparation
        .attachment
        .is_some_and(|id| previews.closed.contains(&id))
        || preparation.cancel.is_cancelled()
    {
        Err(fault("Cancelled", "preview owner detached"))
    } else {
        Ok(())
    }
}
pub(super) async fn prepare_export(shared: &Shared, body: &Value) -> Result<Value, Fault> {
    let preparation = begin(shared, body)?;
    let artifact = shared
        .session
        .export_with_cancel(
            field(body, "selection")?,
            Format::Jsonl,
            preparation.cancel.clone(),
        )
        .await?;
    let id = token()?;
    let result = json!({ "preview_id": id, "artifact": artifact });
    {
        let mut previews = shared
            .management
            .previews
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        active(&previews, &preparation)?;
        if previews.exports.len() >= 8 {
            return Err(fault("PreviewLimit", "discard an unused export preview"));
        }
        previews
            .exports
            .insert(id, (preparation.attachment, artifact));
    }
    Ok(result)
}
pub(super) async fn prepare_copy(shared: &Shared, body: &Value) -> Result<Value, Fault> {
    let preparation = begin(shared, body)?;
    let cli = shared
        .management
        .launch
        .as_ref()
        .ok_or_else(|| fault("Unavailable", "composition unavailable"))?;
    let composition = crate::composition(cli).map_err(|e| fault("InvalidInput", e.to_string()))?;
    let plan = Session::plan_copy(
        composition,
        eden_agent::CopyOptions {
            source: management::managed_path(shared, body, "source")?,
            destination: management::managed_path(shared, body, "destination")?,
            kind: field(body, "kind")?,
            target: body["target"].as_u64(),
            cwd: None,
            public_only: false,
        },
    )
    .await?;
    let id = token()?;
    let result = json!({ "preview_id": id, "plan": plan });
    {
        let mut previews = shared
            .management
            .previews
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        active(&previews, &preparation)?;
        if previews.copies.len() >= 8 {
            return Err(fault("PreviewLimit", "discard an unused copy preview"));
        }
        previews.copies.insert(id, (preparation.attachment, plan));
    }
    Ok(result)
}
fn export(shared: &Shared, body: &Value) -> Result<Artifact, Fault> {
    shared
        .management
        .previews
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .exports
        .get(&field::<String>(body, "preview_id")?)
        .map(|(_, artifact)| artifact.clone())
        .ok_or_else(|| fault("PreviewExpired", "prepare the export again"))
}
pub(super) async fn save(shared: &Shared, body: &Value) -> Result<Value, Fault> {
    save_artifact_file(
        export(shared, body)?,
        management::managed_path(shared, body, "path")?,
    )
    .await?;
    Ok(json!({ "saved": body["path"] }))
}
pub(super) fn publish(shared: &Shared, body: &Value) -> Result<Value, Fault> {
    Ok(json!({
        "run_id": shared.session.publish(PublishRequest {
            artifact: export(shared, body)?,
            confirmed: field(body, "confirmed")?
        })?,
    }))
}
pub(super) async fn apply_copy(shared: &Shared, body: &Value) -> Result<Value, Fault> {
    if body["confirmed"] != true {
        return Err(fault("InvalidInput", "confirm the reviewed copy plan"));
    }
    let (_, plan) = shared
        .management
        .previews
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .copies
        .remove(&field::<String>(body, "preview_id")?)
        .ok_or_else(|| fault("PreviewExpired", "prepare the copy again"))?;
    Ok(json!({ "created": Session::apply_copy(plan).await? }))
}
pub(super) fn discard(shared: &Shared, body: &Value, copy: bool) -> Result<Value, Fault> {
    let id = field::<String>(body, "preview_id")?;
    let mut previews = shared
        .management
        .previews
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if copy {
        previews.copies.remove(&id);
    } else {
        previews.exports.remove(&id);
    }
    Ok(json!({ "discarded": true }))
}
pub(super) async fn detach(shared: &Shared, attachment: u64) {
    let pending = {
        let mut previews = shared
            .management
            .previews
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        previews.closed.insert(attachment);
        previews
            .exports
            .retain(|_, (owner, _)| *owner != Some(attachment));
        previews
            .copies
            .retain(|_, (owner, _)| *owner != Some(attachment));
        previews
            .preparing
            .values()
            .filter(|p| p.attachment == Some(attachment))
            .map(|p| {
                p.cancel.cancel();
                p.finished.clone()
            })
            .collect::<Vec<_>>()
    };
    for mut finished in pending {
        while !*finished.borrow_and_update() {
            if finished.changed().await.is_err() {
                break;
            }
        }
    }
    let mut submissions = shared.submissions.lock().unwrap_or_else(|e| e.into_inner());
    for submission in submissions.0.values_mut() {
        if let Submission::Done { body, result } = submission
            && matches!(
                body["route"].as_str(),
                Some("/delivery/preview" | "/manage/copy/preview")
            )
            && body["body"]["attachment"].as_u64() == Some(attachment)
            && result.is_ok()
        {
            *result = Err(fault("Cancelled", "preview owner detached"));
        }
    }
}

pub(super) fn completed_result(
    shared: &Shared,
    route: &str,
    body: &Value,
    result: Result<Value, Fault>,
) -> Result<Value, Fault> {
    if result.is_ok()
        && matches!(route, "/delivery/preview" | "/manage/copy/preview")
        && body["attachment"].as_u64().is_some_and(|owner| {
            shared
                .management
                .previews
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .closed
                .contains(&owner)
        })
    {
        Err(fault("Cancelled", "preview owner detached"))
    } else {
        result
    }
}
