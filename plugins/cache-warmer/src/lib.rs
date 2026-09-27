//! Optional cache warming through public model and managed-job contracts only.
use eden_plugin_sdk::{
    CallContext, Package, export_plugin, protocol as p,
    serde_json::{self, Value, json},
    tokio,
};
use p::{
    Fault,
    auxiliary::{Request, Snapshot},
};
use serde::Deserialize;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;
const WATCH: &str = "eden.cache-warmer.watch.v1";
const WORK: &str = "eden.cache-warmer.work.v1";
const CONTROL: &str = "eden.cache-warmer.v1";

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Mode {
    #[default]
    Off,
    Streaming,
    Idle,
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    mode: Mode,
    interval_ms: u64,
    ttl_ms: u64,
    safety_ms: u64,
    timeout_ms: u64,
    max_requests: u32,
    max_output_tokens: u32,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Off,
            interval_ms: 60_000,
            ttl_ms: 300_000,
            safety_ms: 15_000,
            timeout_ms: 10_000,
            max_requests: 3,
            max_output_tokens: 1,
        }
    }
}
impl Config {
    fn parse(mut value: Value) -> Result<Self, Fault> {
        if let Some(object) = value.as_object_mut() {
            object.remove(p::environment::CONFIG_KEY);
        }
        let config: Self = serde_json::from_value(if value.is_null() { json!({}) } else { value })
            .map_err(|_| fault("invalid cache-warmer configuration"))?;
        if config.interval_ms == 0
            || config.ttl_ms > 3_600_000
            || config.safety_ms >= config.ttl_ms
            || config.timeout_ms == 0
            || config.timeout_ms > config.safety_ms
            || config.interval_ms >= config.ttl_ms - config.safety_ms
            || config.max_requests == 0
            || config.max_requests > 100
            || config.max_output_tokens == 0
            || config.max_output_tokens > 128
        {
            return Err(fault(
                "interval, TTL safety window, timeout or request budget is invalid",
            ));
        }
        Ok(config)
    }
}
fn fault(message: &str) -> Fault {
    Fault::new("InvalidInput", "cache-warmer", message)
}
struct Schedule {
    snapshot: Snapshot,
    expires: Instant,
    next: Instant,
    remaining: u32,
}
impl Schedule {
    fn new(snapshot: Snapshot, config: &Config, now: Instant) -> Option<Self> {
        let budget = config
            .ttl_ms
            .saturating_sub(config.safety_ms)
            .checked_sub(snapshot.age_ms)?;
        if budget <= config.interval_ms {
            return None;
        }
        Some(Self {
            snapshot,
            expires: now + Duration::from_millis(budget),
            next: now + Duration::from_millis(config.interval_ms),
            remaining: config.max_requests,
        })
    }
    fn take(&mut self, now: Instant, config: &Config) -> Option<Snapshot> {
        if now < self.next || now >= self.expires || self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        // A late wake schedules the next interval from now; it never catches up missed ticks.
        self.next = now + Duration::from_millis(config.interval_ms);
        Some(self.snapshot.clone())
    }
}
#[derive(Default)]
struct State {
    manager: Option<u64>,
    active: Option<u64>,
    status: String,
    requests: u64,
    usage: Value,
    error: Option<Fault>,
}
type Shared = Arc<Mutex<State>>;
fn descriptor() -> p::Descriptor {
    p::Descriptor {
        package: "cache-warmer".into(),
        version: "0.1.0".into(),
        provides: vec![
            p::runtime::READY.into(),
            WATCH.into(),
            WORK.into(),
            CONTROL.into(),
            p::configuration::CONFIGURATION.into(),
        ],
    }
}
fn create(value: Value) -> Result<Package, Fault> {
    let config = Arc::new(Config::parse(value)?);
    let state = Arc::new(Mutex::new(State {
        status: if config.mode == Mode::Off {
            "disabled"
        } else {
            "waiting"
        }
        .into(),
        ..State::default()
    }));
    let ready_config = config.clone();
    let ready_state = state.clone();
    let watch_config = config.clone();
    let watch_state = state.clone();
    let work_config = config.clone();
    let work_state = state.clone();
    Ok(Package::new("cache-warmer")
        .service(p::runtime::READY, move |_: (), cx| {
            let config = ready_config.clone();
            let state = ready_state.clone();
            async move {
                if config.mode != Mode::Off {
                    let job = cx.submit_job(WATCH, &()).await?;
                    state.lock().unwrap_or_else(|e| e.into_inner()).manager = Some(job.id);
                }
                Ok::<(), Fault>(())
            }
        })
        .service(WATCH, move |_: (), cx| {
            let config = watch_config.clone();
            let state = watch_state.clone();
            async move {
                let result = watch(cx, config, state.clone()).await;
                if let Err(error) = &result {
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    state.status = "failed".into();
                    state.error = Some(error.clone());
                }
                result
            }
        })
        .service(WORK, move |snapshot: Snapshot, cx| {
            let config = work_config.clone();
            let state = work_state.clone();
            async move {
                {
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    state.requests += 1;
                    state.status = "running".into();
                }
                let terminal = cx
                    .call_terminal(
                        p::auxiliary::PROVIDER,
                        &Request::Replay {
                            snapshot,
                            streaming: config.mode == Mode::Streaming,
                            max_output_tokens: config.max_output_tokens,
                            max_age_ms: config.ttl_ms - config.safety_ms,
                            timeout_ms: config.timeout_ms,
                        },
                    )
                    .await;
                let reply = decode_reply(terminal);
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                let reply = match reply {
                    Ok(reply) => reply,
                    Err(error) => {
                        state.status = "cleanup_failed".into();
                        state.error = Some(error.clone());
                        return Err(error);
                    }
                };
                match reply {
                    Ok(reply) => {
                        state.usage = reply.usage;
                        state.error = None;
                        state.status = "waiting".into();
                    }
                    Err(error) => {
                        state.status = "paused".into();
                        state.error = Some(error);
                    }
                }
                // Intentionally discard every returned item, including executable tool calls.
                Ok::<(), Fault>(())
            }
        })
        .service(CONTROL, move |input: Value, cx| {
            let state = state.clone();
            async move {
                if input["op"] == "cancel" {
                    let manager = state.lock().unwrap_or_else(|e| e.into_inner()).manager;
                    if let Some(job) = manager {
                        cx.cancel_job(job).await?;
                        clean_join(cx.join_job(job).await?)?;
                        cx.forget_job(job).await?;
                        state.lock().unwrap_or_else(|e| e.into_inner()).manager = None;
                    }
                    stop_active(&cx, &state).await?;
                    state.lock().unwrap_or_else(|e| e.into_inner()).status = "cancelled".into();
                } else if input["op"] != "status" {
                    return Err(fault("expected status or cancel"));
                }
                let state = state.lock().unwrap_or_else(|e| e.into_inner());
                Ok(json!({
                    "status": state.status,
                    "requests": state.requests,
                    "usage": state.usage,
                    "error": state.error,
                    "job": state.active,
                }))
            }
        })
        .service(
            p::configuration::CONFIGURATION,
            |request: p::configuration::PluginRequest, _| async move {
                use p::configuration::{Description, FieldError, PluginRequest, Validation};
                match request {
                    PluginRequest::Describe => Ok(json!(Description {
                        schema: Some(json!({
                            "type": "object",
                            "properties": {
                                "mode": { "type": "string", "enum": ["off", "streaming", "idle"] },
                                "interval_ms": { "type": "integer", "minimum": 1 },
                                "ttl_ms": { "type": "integer", "minimum": 1, "maximum": 3600000 },
                                "safety_ms": { "type": "integer", "minimum": 1 },
                                "timeout_ms": { "type": "integer", "minimum": 1 },
                                "max_requests": { "type": "integer", "minimum": 1, "maximum": 100 },
                                "max_output_tokens": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "maximum": 128,
                                },
                            },
                            "additionalProperties": false,
                        })),
                        defaults: json!({
                            "mode": "off",
                            "interval_ms": 60000,
                            "ttl_ms": 300000,
                            "safety_ms": 15000,
                            "timeout_ms": 10000,
                            "max_requests": 3,
                            "max_output_tokens": 1,
                        }),
                        description: Some(
                            "Explicit streaming or idle warming; all changes use managed local \
                             restart"
                                .into()
                        ),
                        ..Description::default()
                    })),
                    PluginRequest::Validate { config } => Ok(json!(Validation {
                        errors: Config::parse(config)
                            .err()
                            .map(|e| FieldError {
                                path: "".into(),
                                code: e.code,
                                message: e.message
                            })
                            .into_iter()
                            .collect()
                    })),
                    PluginRequest::Update { .. } => {
                        Err(fault("configuration requires local restart"))
                    }
                }
            },
        ))
}
fn decode_reply(
    terminal: Result<p::Terminal, Fault>,
) -> Result<Result<p::coding::ModelReply, Fault>, Fault> {
    let terminal = match terminal {
        Ok(terminal) => terminal,
        Err(error) => return Ok(Err(error)),
    };
    if let Some(error) = terminal.cleanup_errors.first() {
        return Err(error.clone());
    }
    Ok(terminal.into_result().and_then(|value| {
        serde_json::from_value(value).map_err(|_| fault("invalid auxiliary reply"))
    }))
}
fn retain_schedule(previous: Option<Schedule>, current: Option<Schedule>) -> Option<Schedule> {
    match (previous, current) {
        (Some(previous), Some(current))
            if previous.snapshot.owner == current.snapshot.owner
                && previous.snapshot.token == current.snapshot.token
                && previous.snapshot.revision == current.snapshot.revision =>
        {
            Some(previous)
        }
        (_, current) => current,
    }
}
fn clean_join(status: p::runtime::JobStatus) -> Result<(), Fault> {
    let terminal = status.terminal.ok_or_else(|| fault("job did not settle"))?;
    if let Some(error) = terminal.cleanup_errors.into_iter().next() {
        return Err(error);
    }
    match terminal.outcome {
        p::Outcome::Failed(error) => Err(error),
        _ => Ok(()),
    }
}
async fn stop_active(cx: &CallContext, state: &Shared) -> Result<(), Fault> {
    let job = state.lock().unwrap_or_else(|e| e.into_inner()).active;
    if let Some(job) = job {
        cx.cancel_job(job).await?;
        if let Err(error) = clean_join(cx.join_job(job).await?) {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            state.status = "cleanup_failed".into();
            state.error = Some(error.clone());
            return Err(error);
        }
        cx.forget_job(job).await?;
        state.lock().unwrap_or_else(|e| e.into_inner()).active = None;
    }
    Ok(())
}
async fn bootstrap(cx: &CallContext, config: &Config) -> Result<(u64, Option<Schedule>), Fault> {
    let position: p::runtime::EventPosition = cx
        .call(p::runtime::HOST, &p::runtime::HostRequest::EventPosition)
        .await?;
    let latest: Option<Snapshot> = cx.call(p::auxiliary::PROVIDER, &Request::Latest).await?;
    let schedule = latest
        .filter(|snapshot| {
            config.mode == Mode::Idle || position.foreground_run == Some(snapshot.origin_run)
        })
        .and_then(|snapshot| Schedule::new(snapshot, config, Instant::now()));
    Ok((position.cursor, schedule))
}
async fn watch(cx: CallContext, config: Arc<Config>, state: Shared) -> Result<(), Fault> {
    let (mut cursor, mut schedule) = bootstrap(&cx, &config).await?;
    loop {
        // Retain this single subscription future across timer wakes; dropping a host-call
        // waiter does not cancel its native bridge until its scope settles.
        let events = cx.events_after(
            cursor,
            vec![
                "auxiliary_snapshot".into(),
                "auxiliary_unavailable".into(),
                "accepted".into(),
                "settled".into(),
                "job_settled".into(),
            ],
        );
        tokio::pin!(events);
        loop {
            let next = schedule
                .as_ref()
                .filter(|s| s.remaining > 0 && s.next < s.expires)
                .map(|s| s.next);
            let timer = async {
                match next {
                    Some(next) => tokio::time::sleep_until(next).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                biased;
                batch = &mut events => {
                    let batch = match batch {
                        Ok(batch) => batch,
                        Err(error) if error.code == "Lagged" => {
                            stop_active(&cx, &state).await?;
                            let (position, current) = bootstrap(&cx, &config).await?;
                            cursor = position;
                            schedule = retain_schedule(schedule, current);
                            break;
                        }
                        Err(error) => return Err(error),
                    };
                    cursor = batch.cursor;
                    for event in batch.events {
                        if event.kind == "auxiliary_snapshot" {
                            stop_active(&cx, &state).await?;
                            let snapshot: Snapshot = serde_json::from_value(event.payload)
                                .map_err(|_| fault("invalid snapshot observation"))?;
                            // Query current provider age: an event may have waited in the ledger.
                            let current: Option<Snapshot> =
                                cx.call(p::auxiliary::PROVIDER, &Request::Latest).await?;
                            schedule = current
                                .filter(|current| {
                                    current.token == snapshot.token
                                        && current.owner == snapshot.owner
                                        && current.revision == snapshot.revision
                                })
                                .and_then(|s| Schedule::new(s, &config, Instant::now()));
                        } else if event.kind == "auxiliary_unavailable" {
                            schedule = None;
                            stop_active(&cx, &state).await?;
                            state.lock().unwrap_or_else(|e| e.into_inner()).status =
                                "unsupported".into();
                        } else if event.kind == "accepted" && event.payload["management"] == false
                            || event.kind == "settled"
                                && config.mode == Mode::Streaming
                                && schedule
                                    .as_ref()
                                    .is_some_and(|s| s.snapshot.origin_run == event.run_id)
                        {
                            schedule = None;
                            stop_active(&cx, &state).await?;
                        } else if event.kind == "job_settled" {
                            let job = event.payload["job"].as_u64();
                            let active = state.lock().unwrap_or_else(|e| e.into_inner()).active;
                            if job.is_some() && job == active {
                                stop_active(&cx, &state).await?;
                            }
                        }
                    }
                    break;
                },
                _ = timer => {
                    let active = state.lock().unwrap_or_else(|e| e.into_inner()).active;
                    if let Some(job) = active {
                        let status = cx.inspect_job(job).await?;
                        if status.terminal.is_none() {
                            if let Some(schedule) = schedule.as_mut() {
                                schedule.next =
                                    Instant::now() + Duration::from_millis(config.interval_ms);
                            }
                            continue;
                        }
                        stop_active(&cx, &state).await?;
                    }
                    if let Some(snapshot) = schedule
                        .as_mut()
                        .and_then(|s| s.take(Instant::now(), &config))
                    {
                        stop_active(&cx, &state).await?;
                        let job = cx.submit_job(WORK, &snapshot).await?;
                        state.lock().unwrap_or_else(|e| e.into_inner()).active = Some(job.id);
                    } else {
                        schedule = None;
                        state.lock().unwrap_or_else(|e| e.into_inner()).status = "expired".into();
                    }
                }
            }
        }
    }
}
export_plugin!(descriptor, create);

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(age_ms: u64) -> Snapshot {
        Snapshot {
            owner: p::runtime::InstanceIdentity {
                id: "provider".into(),
                generation: 1,
            },
            revision: 1,
            token: 1,
            origin_run: 1,
            age_ms,
        }
    }
    #[tokio::test(start_paused = true)]
    async fn missed_window_never_sends_or_renews_deadline() {
        let config = Config::default();
        let now = Instant::now();
        let mut schedule = Schedule::new(snapshot(0), &config, now).unwrap();
        let deadline = schedule.expires;
        tokio::time::advance(Duration::from_millis(config.interval_ms)).await;
        assert!(schedule.take(Instant::now(), &config).is_some());
        assert_eq!(schedule.expires, deadline);
        tokio::time::advance(Duration::from_millis(config.ttl_ms)).await;
        assert!(schedule.take(Instant::now(), &config).is_none());
        assert!(Schedule::new(snapshot(config.ttl_ms), &config, Instant::now()).is_none());
    }
    #[test]
    fn descriptor_and_factory_order_match() {
        assert_eq!(
            json!(descriptor()),
            json!(create(json!({})).unwrap().descriptor())
        );
    }
    #[test]
    fn downstream_cleanup_failure_remains_distinct_from_provider_failure() {
        let terminal = p::Terminal {
            outcome: p::Outcome::Completed(json!({ "items": [], "usage": {} })),
            cleanup_errors: vec![Fault::new(
                "CustomCleanupError",
                "provider",
                "cleanup failed",
            )],
            partial_result: None,
        };
        assert_eq!(
            decode_reply(Ok(terminal)).unwrap_err().code,
            "CustomCleanupError"
        );
        assert!(
            decode_reply(Ok(p::Terminal::failed(Fault::new(
                "ProviderFailure",
                "provider",
                "request failed"
            ))))
            .unwrap()
            .is_err()
        );
    }
    #[test]
    fn subscription_resync_does_not_restore_spent_request_budget() {
        let config = Config::default();
        let now = Instant::now();
        let mut old = Schedule::new(snapshot(0), &config, now).unwrap();
        old.remaining = 0;
        let deadline = old.expires;
        let restored = retain_schedule(
            Some(old),
            Schedule::new(snapshot(0), &config, now + Duration::from_secs(1)),
        )
        .unwrap();
        assert_eq!(restored.remaining, 0);
        assert_eq!(restored.expires, deadline);
    }
    #[test]
    fn cleanup_failure_is_not_a_successful_cancel() {
        let status = p::runtime::JobStatus {
            id: 1,
            owner: snapshot(0).owner,
            terminal: Some(p::Terminal {
                outcome: p::Outcome::Cancelled,
                cleanup_errors: vec![Fault::new("CleanupFailure", "author", "barrier failed")],
                partial_result: None,
            }),
        };
        assert_eq!(clean_join(status).unwrap_err().code, "CleanupFailure");
    }
    #[test]
    fn disabled_by_default_and_invalid_budget_is_rejected() {
        assert!(Config::parse(json!({})).unwrap().mode == Mode::Off);
        assert!(Config::parse(json!({ "timeout_ms": 16000 })).is_err());
        assert!(Config::parse(json!({ "mode": "idle", "max_requests": 0 })).is_err());
    }
}
