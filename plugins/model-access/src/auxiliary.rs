//! Prepared payloads remain private to the provider incarnation and contain no credentials.
use crate::{targeted, wire::failure};
use eden_plugin_sdk::{CallContext, Package};
use eden_protocol::{
    Fault,
    auxiliary::{self, Request, Snapshot},
    coding::{ModelInput, ModelReply},
    models::{CREDENTIAL_SOURCE, CredentialReply, CredentialRequest, ModelTarget},
    runtime::{HOST, HostRequest},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
#[derive(Clone)]
struct Saved {
    snapshot: Snapshot,
    session: u64,
    created: Instant,
    target: ModelTarget,
    prepared: targeted::Prepared,
}
#[derive(Default)]
pub(crate) struct State(Mutex<Option<Saved>>);
fn supported(target: &ModelTarget) -> bool {
    target.api == "openai-completions" && matches!(target.provider.as_str(), "openai" | "deepseek")
}
async fn revision(cx: &CallContext) -> Result<u64, Fault> {
    cx.call(HOST, &HostRequest::SnapshotRevision).await
}
pub(crate) async fn capture(
    state: &State,
    cx: &CallContext,
    target: &ModelTarget,
    prepared: &targeted::Prepared,
) -> Result<Option<Snapshot>, Fault> {
    let Some(identity) = cx.identity() else {
        return Ok(None);
    };
    if !supported(target) {
        return Ok(None);
    }
    let admitted: u64 = cx
        .call(HOST, &HostRequest::InvocationSnapshotRevision)
        .await?;
    if admitted != revision(cx).await? {
        return Ok(None);
    }
    let snapshot = Snapshot {
        owner: identity.owner.clone(),
        revision: admitted,
        token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
        origin_run: cx.run_id(),
        age_ms: 0,
    };
    *state
        .0
        .lock()
        .map_err(|_| failure("snapshot state unavailable"))? = Some(Saved {
        snapshot: snapshot.clone(),
        session: cx.session_id(),
        created: Instant::now(),
        target: target.clone(),
        prepared: prepared.clone(),
    });
    cx.emit("auxiliary_snapshot", json!(snapshot))?;
    Ok(Some(snapshot))
}
fn age(saved: &Saved) -> u64 {
    saved
        .created
        .elapsed()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
fn stale() -> Fault {
    Fault::new(
        "StaleSnapshot",
        "model-access",
        "prepared snapshot is stale or expired",
    )
}
fn validate(saved: &Saved, snapshot: &Snapshot, max_age_ms: u64) -> Result<(), Fault> {
    if saved.snapshot.owner != snapshot.owner
        || saved.snapshot.token != snapshot.token
        || saved.snapshot.revision != snapshot.revision
        || saved.snapshot.origin_run != snapshot.origin_run
        || max_age_ms == 0
        || max_age_ms > 3_600_000
        || age(saved) >= max_age_ms
    {
        return Err(stale());
    }
    Ok(())
}
async fn credentials(
    cx: &CallContext,
    target: &ModelTarget,
    purpose: &str,
) -> Result<CredentialReply, Fault> {
    cx.call(
        CREDENTIAL_SOURCE,
        &CredentialRequest {
            provider: target.provider.clone(),
            explicit: None,
            purpose: purpose.into(),
        },
    )
    .await
}
fn budget(target: &ModelTarget, limit: u32) -> Result<(), Fault> {
    let maximum = target.limits.max_output_tokens.min(4096);
    if limit == 0 || limit > maximum {
        return Err(failure(
            "auxiliary output budget exceeds the model limit or 4096 tokens",
        ));
    }
    Ok(())
}
async fn execute(
    cx: &CallContext,
    purpose: &str,
    timeout_ms: u64,
    target: &ModelTarget,
    prepared: &targeted::Prepared,
    snapshot: Option<&Snapshot>,
) -> Result<ModelReply, Fault> {
    if timeout_ms == 0 || timeout_ms > 120_000 {
        return Err(failure("auxiliary timeout must be between 1 and 120000 ms"));
    }
    let call_id = cx.identity().map_or_else(
        || NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
        |identity| identity.call,
    );
    let emit = |kind: &str, mut payload: Value| {
        payload["purpose"] = json!(purpose);
        payload["call_id"] = json!(call_id);
        payload["owner"] = json!(cx.identity().map(|identity| &identity.owner));
        payload["snapshot"] = json!(snapshot);
        cx.emit(kind, payload)
    };
    emit("auxiliary_started", json!({}))?;
    let cancel = cx.scope.cancellation();
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Fault::new(
            "Cancelled",
            "model-access",
            "auxiliary request cancelled"
        )),
        result = tokio::time::timeout(
                Duration::from_millis(timeout_ms),
                async {
                    let credential = credentials(cx, target, purpose).await?;
                    // Chat decoding needs only the frozen target; no logical input or credentials are retained.
                    let input = ModelInput {
                        target: None,
                        items: vec![],
                        tools: vec![],
                        max_output_tokens: None,
                    };
                    targeted::send_auxiliary(target, &credential, &input, prepared).await
                }
            ) => result.unwrap_or_else(|_| Err(Fault::new(
                "Timeout",
                "model-access",
                "auxiliary request timed out"
            ))),
    };
    if let Ok(reply) = &result {
        emit("auxiliary_usage", json!({ "usage": reply.usage }))?;
    }
    emit(
        "auxiliary_finished",
        json!({ "error": result.as_ref().err() }),
    )?;
    result
}
pub(crate) fn register(package: Package, state: Arc<State>) -> Package {
    package.service(auxiliary::PROVIDER, move |request: Request, cx| {
        let state = state.clone();
        async move {
            match request {
                Request::Latest => {
                    let current = revision(&cx).await?;
                    let saved = state
                        .0
                        .lock()
                        .map_err(|_| failure("snapshot state unavailable"))?;
                    let snapshot = saved
                        .as_ref()
                        .filter(|saved| {
                            saved.session == cx.session_id()
                                && saved.snapshot.revision == current
                                && cx
                                    .identity()
                                    .is_some_and(|identity| identity.owner == saved.snapshot.owner)
                        })
                        .map(|saved| {
                            let mut snapshot = saved.snapshot.clone();
                            snapshot.age_ms = age(saved);
                            snapshot
                        });
                    Ok(json!(snapshot))
                }
                Request::Prepare { input } => {
                    if cx.run_id() == 0 {
                        return Err(failure("snapshot preparation requires a foreground run"));
                    }
                    let Some(target) = input.target.as_ref().filter(|target| supported(target))
                    else {
                        return Ok(Value::Null);
                    };
                    let prepared = targeted::prepare(target, &input)?;
                    Ok(json!(capture(&state, &cx, target, &prepared).await?))
                }
                Request::Replay {
                    streaming: _,
                    snapshot,
                    max_output_tokens,
                    max_age_ms,
                    timeout_ms,
                } => {
                    let current = revision(&cx).await?;
                    let mut saved = state
                        .0
                        .lock()
                        .map_err(|_| failure("snapshot state unavailable"))?
                        .clone()
                        .ok_or_else(stale)?;
                    validate(&saved, &snapshot, max_age_ms)?;
                    if current != snapshot.revision
                        || saved.session != cx.session_id()
                        || !cx
                            .identity()
                            .is_some_and(|identity| identity.owner == snapshot.owner)
                    {
                        return Err(stale());
                    }
                    budget(&saved.target, max_output_tokens)?;
                    saved.prepared.set_output_limit(max_output_tokens)?;
                    if timeout_ms == 0 || timeout_ms > 120_000 {
                        return Err(failure("auxiliary timeout must be between 1 and 120000 ms"));
                    }
                    let remaining = max_age_ms.saturating_sub(age(&saved));
                    if remaining == 0 {
                        return Err(stale());
                    }
                    let reply = execute(
                        &cx,
                        "cache_warm",
                        timeout_ms.min(remaining),
                        &saved.target,
                        &saved.prepared,
                        Some(&snapshot),
                    )
                    .await?;
                    let latest = state
                        .0
                        .lock()
                        .map_err(|_| failure("snapshot state unavailable"))?
                        .clone()
                        .ok_or_else(stale)?;
                    validate(&latest, &snapshot, max_age_ms)?;
                    if revision(&cx).await? != snapshot.revision {
                        return Err(stale());
                    }
                    Ok(json!(reply))
                }
                Request::Generate {
                    purpose,
                    input,
                    timeout_ms,
                } => {
                    if purpose.trim().is_empty()
                        || purpose.trim().eq_ignore_ascii_case("foreground")
                    {
                        return Err(failure(
                            "auxiliary purpose must be nonempty and cannot be foreground",
                        ));
                    }
                    if input.max_output_tokens.is_none_or(|limit| limit == 0) {
                        return Err(failure(
                            "auxiliary generation requires a positive output budget",
                        ));
                    }
                    let target = input
                        .target
                        .as_ref()
                        .filter(|target| supported(target))
                        .ok_or_else(|| {
                            Fault::new(
                                "Unsupported",
                                "model-access",
                                "auxiliary target is not supported",
                            )
                        })?;
                    budget(target, input.max_output_tokens.unwrap_or_default())?;
                    let prepared = targeted::prepare(target, &input)?;
                    Ok(json!(
                        execute(&cx, &purpose, timeout_ms, target, &prepared, None).await?
                    ))
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn saved() -> Saved {
        let mut target = crate::projection::test_target("openai-completions");
        target.provider = "openai".into();
        let input = ModelInput {
            target: Some(target.clone()),
            items: vec![],
            tools: vec![],
            max_output_tokens: None,
        };
        Saved {
            snapshot: Snapshot {
                owner: eden_protocol::runtime::InstanceIdentity {
                    id: "provider".into(),
                    generation: 2,
                },
                revision: 3,
                token: 4,
                origin_run: 5,
                age_ms: 0,
            },
            session: 6,
            created: Instant::now(),
            prepared: targeted::prepare(&target, &input).unwrap(),
            target,
        }
    }
    #[test]
    fn stale_replaced_foreign_and_expired_handles_are_rejected() {
        let mut saved = saved();
        validate(&saved, &saved.snapshot, 1000).unwrap();
        for field in ["token", "revision", "generation", "origin_run"] {
            let mut snapshot = saved.snapshot.clone();
            match field {
                "token" => snapshot.token += 1,
                "revision" => snapshot.revision += 1,
                "generation" => snapshot.owner.generation += 1,
                _ => snapshot.origin_run += 1,
            }
            assert_eq!(
                validate(&saved, &snapshot, 1000).unwrap_err().code,
                "StaleSnapshot"
            );
        }
        saved.created = Instant::now() - Duration::from_secs(2);
        let mut forged = saved.snapshot.clone();
        forged.age_ms = 0;
        assert!(validate(&saved, &forged, 1000).is_err());
        assert!(validate(&saved, &forged, u64::MAX).is_err());
    }
    #[test]
    fn auxiliary_budgets_and_provider_support_are_bounded() {
        let mut target = saved().target;
        assert!(supported(&target));
        assert!(budget(&target, 0).is_err());
        assert!(budget(&target, 2049).is_err());
        assert!(budget(&target, 1).is_ok());
        target.provider = "custom-openai-compatible".into();
        assert!(!supported(&target));
    }
}
